use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::daemon::state::{DaemonState, TrackedProcess};
use crate::daemon::stream::spawn_stream_thread;
use crate::error::Result;
use crate::protocol::{DaemonRequest, DaemonResponse, ErrorCode, Request, Response};
use crate::ssh::serve_request_auto_deploy;
use crate::{base64_decode, encode_bytes};

/// The object a daemon control command produced.
///
/// Built here, printed by the CLI. The contract allows exactly one JSON object
/// per invocation, and only the binary knows whether that object should be
/// compact or pretty — so this module answers with data and never writes to a
/// stream itself.
fn outcome(action: &str, running: bool, changed: bool) -> serde_json::Value {
    serde_json::json!({
        "type": "daemon",
        "action": action,
        "running": running,
        "changed": changed,
    })
}

/// Read the recorded daemon pid, if the file is there and parses.
fn recorded_pid() -> Option<u32> {
    fs::read_to_string(super::pid_path())
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Start the daemon: fork, set up Unix socket, serve requests.
///
/// Idempotent: a daemon that is already running is the requested state, not an
/// error — the caller asked for "running", and it is.
pub fn start_daemon() -> Result<serde_json::Value> {
    let sock_path = super::socket_path();
    let pid_path = super::pid_path();
    let base = super::local_base();

    // Check for stale socket
    if sock_path.exists() {
        if super::is_running() {
            let mut value = outcome("start", true, false);
            if let (Some(obj), Some(pid)) = (value.as_object_mut(), recorded_pid()) {
                obj.insert("pid".into(), serde_json::json!(pid));
            }
            return Ok(value);
        }
        // Stale socket — remove it
        let _ = fs::remove_file(&sock_path);
    }

    // Ensure base dir exists with correct permissions (0700)
    let app_dir = crate::process::remote_base();
    crate::process::ensure_base_dir(&app_dir)?;
    fs::create_dir_all(&base)?;

    // Fork to daemonize
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => Err(crate::error::RemExecError::Other(format!(
            "fork failed: {}",
            std::io::Error::last_os_error()
        ))),
        0 => {
            // Child: become the daemon
            unsafe { libc::setsid() };

            // Redirect stdio to /dev/null
            let devnull_path = std::ffi::CString::new("/dev/null").unwrap();
            let devnull = unsafe { libc::open(devnull_path.as_ptr(), libc::O_RDWR) };
            if devnull >= 0 {
                unsafe {
                    libc::dup2(devnull, 0);
                    libc::dup2(devnull, 1);
                    // Keep stderr for logging
                    if devnull > 2 {
                        libc::close(devnull);
                    }
                }
            }

            // Write PID file
            let _ = fs::write(&pid_path, std::process::id().to_string());

            // Run the server
            run_server(&sock_path, base);
            let _ = fs::remove_file(&sock_path);
            let _ = fs::remove_file(&pid_path);
            std::process::exit(0);
        }
        child_pid => {
            // Parent: wait briefly, verify daemon is up, report.
            std::thread::sleep(std::time::Duration::from_millis(200));
            if !super::is_running() {
                return Err(crate::error::RemExecError::Other(
                    "daemon failed to start (socket never appeared)".to_string(),
                ));
            }
            let mut value = outcome("start", true, true);
            if let Some(obj) = value.as_object_mut() {
                obj.insert("pid".into(), serde_json::json!(child_pid));
            }
            Ok(value)
        }
    }
}

/// Stop the daemon by sending a stop request.
///
/// Idempotent for the same reason as [`start_daemon`]: "not running" is the
/// state the caller asked for.
pub fn stop_daemon() -> Result<serde_json::Value> {
    let request = DaemonRequest::DaemonStop;
    match super::send_request(&request) {
        Ok(_) => Ok(outcome("stop", false, true)),
        Err(crate::error::RemExecError::DaemonNotRunning) => Ok(outcome("stop", false, false)),
        Err(e) => Err(e),
    }
}

/// Report whether the daemon is running, with its own status payload when it is.
pub fn daemon_status() -> Result<serde_json::Value> {
    let request = DaemonRequest::DaemonStatus;
    match super::send_request(&request) {
        Ok(resp) => {
            let mut value = outcome("status", true, false);
            if let (Some(obj), DaemonResponse::Ok { data }) = (value.as_object_mut(), resp) {
                obj.insert("detail".into(), data);
            }
            Ok(value)
        }
        Err(crate::error::RemExecError::DaemonNotRunning) => Ok(outcome("status", false, false)),
        Err(e) => Err(e),
    }
}

fn run_server(sock_path: &Path, base: std::path::PathBuf) {
    let listener = match UnixListener::bind(sock_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to bind socket: {e}");
            return;
        }
    };

    // bind(2) applies the umask, so pin the mode explicitly. The parent dir is
    // already 0700 via ensure_base_dir; this closes the window where the base
    // was created by an earlier ssh_command call under a laxer umask.
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = fs::set_permissions(sock_path, fs::Permissions::from_mode(0o600)) {
            eprintln!("failed to restrict socket permissions: {e}");
            let _ = fs::remove_file(sock_path);
            return;
        }
    }

    let state = Arc::new(Mutex::new(DaemonState::new(base)));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Set a timeout so we can check the stop flag periodically
    listener.set_nonblocking(false).unwrap_or(());

    for stream_result in listener.incoming() {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }

        let stream = match stream_result {
            Ok(s) => s,
            Err(_) => continue,
        };

        let state = Arc::clone(&state);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            handle_connection(stream, state, stop);
        });
    }
}

fn handle_connection(
    mut stream: UnixStream,
    state: Arc<Mutex<DaemonState>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut buf = Vec::new();
    if stream.read_to_end(&mut buf).is_err() {
        return;
    }

    let request: DaemonRequest = match serde_json::from_slice(&buf) {
        Ok(r) => r,
        Err(e) => {
            let resp = DaemonResponse::Error {
                message: format!("invalid request: {e}"),
            };
            let _ = stream.write_all(&serde_json::to_vec(&resp).unwrap_or_default());
            return;
        }
    };

    let response = dispatch(request, &state, &stop);
    let _ = stream.write_all(&serde_json::to_vec(&response).unwrap_or_default());
}

fn dispatch(
    request: DaemonRequest,
    state: &Arc<Mutex<DaemonState>>,
    stop: &Arc<std::sync::atomic::AtomicBool>,
) -> DaemonResponse {
    match request {
        DaemonRequest::Run {
            host,
            command,
            cwd,
            env,
            timeout_ms,
            stdin_b64,
            keep_stdin_open,
            ephemeral,
        } => {
            let body = match stdin_b64.as_deref() {
                Some(s) => match base64_decode(s) {
                    Ok(b) => b,
                    Err(e) => {
                        return wrap_response(Response::error_code(
                            ErrorCode::BadRequest,
                            format!("invalid base64 in stdin: {e}"),
                        ));
                    }
                },
                None => Vec::new(),
            };
            forward(
                &host,
                &Request::Run {
                    command,
                    cwd,
                    env,
                    timeout_ms,
                    keep_stdin_open,
                    ephemeral,
                },
                &body,
            )
        }
        DaemonRequest::Start {
            host,
            command,
            cwd,
            env,
        } => handle_start(&host, &command, cwd, env, state),
        DaemonRequest::Wait {
            host,
            id,
            timeout_ms,
        } => forward(&host, &Request::Wait { id, timeout_ms }, &[]),
        DaemonRequest::Status { host, id } => forward(&host, &Request::Status { id }, &[]),
        DaemonRequest::Stdout {
            host,
            id,
            offset,
            limit,
        } => handle_read(&host, &id, "stdout", offset, limit, state),
        DaemonRequest::Stderr {
            host,
            id,
            offset,
            limit,
        } => handle_read(&host, &id, "stderr", offset, limit, state),
        DaemonRequest::Write { host, id, data_b64 } => {
            let body = match base64_decode(&data_b64) {
                Ok(b) => b,
                Err(e) => {
                    return wrap_response(Response::error_code(
                        ErrorCode::BadRequest,
                        format!("invalid base64 in write data: {e}"),
                    ));
                }
            };
            forward(&host, &Request::Write { id }, &body)
        }
        DaemonRequest::CloseStdin { host, id } => forward(&host, &Request::CloseStdin { id }, &[]),
        DaemonRequest::Kill { host, id } => forward(&host, &Request::Kill { id }, &[]),
        DaemonRequest::List { host } => forward(&host, &Request::List, &[]),
        DaemonRequest::Clean { host } => {
            // Clean remote and local
            let resp = forward(&host, &Request::Clean, &[]);
            // Also clean local cached files for this host
            if let Ok(mut st) = state.lock() {
                if let Some(host_state) = st.hosts.get_mut(&host) {
                    host_state.processes.retain(|_, _| false);
                }
                let host_dir = st.host_dir(&host);
                let _ = fs::remove_dir_all(&host_dir);
            }
            resp
        }
        DaemonRequest::Deploy { host } => match crate::deploy::deploy_to_host(&host) {
            Ok(result) => DaemonResponse::Ok {
                data: serde_json::json!({
                    "type": "deployed",
                    "host": result.host,
                    "arch": result.arch,
                    "version": result.version,
                }),
            },
            Err(e) => DaemonResponse::Error {
                message: e.to_string(),
            },
        },
        DaemonRequest::DaemonStatus => {
            let st = state.lock().unwrap();
            let (hosts, procs) = st.summary();
            DaemonResponse::Ok {
                data: serde_json::json!({
                    "type": "daemon_status",
                    "hosts": hosts,
                    "processes": procs,
                }),
            }
        }
        DaemonRequest::DaemonStop => {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            // Connect to our own socket to unblock the accept loop
            let sock = super::socket_path();
            let _ = std::os::unix::net::UnixStream::connect(&sock);
            DaemonResponse::Ok {
                data: serde_json::json!({"type": "stopped"}),
            }
        }
    }
}

/// Start a process remotely and begin streaming its output locally.
fn handle_start(
    host: &str,
    command: &[String],
    cwd: Option<String>,
    env: std::collections::BTreeMap<String, String>,
    state: &Arc<Mutex<DaemonState>>,
) -> DaemonResponse {
    let request = Request::Start {
        command: command.to_vec(),
        cwd,
        env,
    };
    let response = match serve_request_auto_deploy(host, &request, &[]) {
        Ok(r) => r,
        Err(e) => {
            return DaemonResponse::Error {
                message: e.to_string(),
            };
        }
    };

    // Extract the process ID from the response
    if let Response::Started { ref id } = response {
        let mut st = state.lock().unwrap();
        let local_dir = st.local_dir(host, id);
        let _ = fs::create_dir_all(&local_dir);

        // Also write a local status file
        let _ = fs::write(local_dir.join("status"), "running");

        // Spawn streaming threads for stdout and stderr
        let stdout_thread = spawn_stream_thread(
            host.to_string(),
            id.clone(),
            "stdout".to_string(),
            local_dir.join("stdout"),
        );
        let stderr_thread = spawn_stream_thread(
            host.to_string(),
            id.clone(),
            "stderr".to_string(),
            local_dir.join("stderr"),
        );

        let host_state = st.host_mut(host);
        host_state.processes.insert(
            id.clone(),
            TrackedProcess {
                id: id.clone(),
                local_dir,
                stdout_thread: Some(stdout_thread),
                stderr_thread: Some(stderr_thread),
            },
        );
    }

    wrap_response(response)
}

/// Default read limit for daemon-side reads (same as remote).
const DEFAULT_READ_LIMIT: u64 = 1024 * 1024;

/// Read from local cached file if available, otherwise forward to SSH.
fn handle_read(
    host: &str,
    id: &str,
    stream_name: &str,
    offset: Option<u64>,
    limit: Option<u64>,
    state: &Arc<Mutex<DaemonState>>,
) -> DaemonResponse {
    let st = state.lock().unwrap();
    let local_path = st.local_dir(host, id).join(stream_name);
    drop(st);

    if local_path.exists() {
        // Read from local cached file (bounded)
        use std::io::{Read, Seek, SeekFrom};
        let file_size = fs::metadata(&local_path).map(|m| m.len()).unwrap_or(0);
        match fs::File::open(&local_path) {
            Ok(mut file) => {
                let offset = offset.unwrap_or(0);
                if offset > 0 {
                    let _ = file.seek(SeekFrom::Start(offset));
                }
                let limit = limit.unwrap_or(DEFAULT_READ_LIMIT);
                let to_read = limit.min(file_size.saturating_sub(offset)) as usize;
                let mut data = vec![0u8; to_read];
                let n = file.read(&mut data).unwrap_or(0);
                data.truncate(n);
                let (data, encoding) = encode_bytes(&data);
                let response = Response::Output {
                    data,
                    encoding,
                    offset,
                    size: file_size,
                };
                wrap_response(response)
            }
            Err(e) => DaemonResponse::Error {
                message: format!("failed to read local cache: {e}"),
            },
        }
    } else {
        // Not cached — forward to the remote over the serve transport.
        forward(
            host,
            &Request::Read {
                id: id.to_string(),
                stream: stream_name.to_string(),
                offset,
                limit,
            },
            &[],
        )
    }
}

/// Forward a request to the remote host over the serve transport.
fn forward(host: &str, request: &Request, body: &[u8]) -> DaemonResponse {
    match serve_request_auto_deploy(host, request, body) {
        Ok(r) => wrap_response(r),
        Err(e) => DaemonResponse::Error {
            message: e.to_string(),
        },
    }
}

fn wrap_response(response: Response) -> DaemonResponse {
    match serde_json::to_value(&response) {
        Ok(v) => DaemonResponse::Ok { data: v },
        Err(e) => DaemonResponse::Error {
            message: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn write_with_invalid_base64_fails_loud_without_forwarding() {
        // Corrupt base64 must not become an empty body silently forwarded as a
        // success — it returns a typed bad_request error (and never reaches SSH).
        let state = Arc::new(Mutex::new(DaemonState::new(std::path::PathBuf::from(
            "/tmp/rxd-test-unused",
        ))));
        let stop = Arc::new(AtomicBool::new(false));
        let req = DaemonRequest::Write {
            host: "unused".to_string(),
            id: "0123abcd".to_string(),
            data_b64: "!!!!".to_string(),
        };
        match dispatch(req, &state, &stop) {
            DaemonResponse::Ok { data } => {
                assert_eq!(data.get("type").and_then(|v| v.as_str()), Some("error"));
                assert_eq!(
                    data.get("code").and_then(|v| v.as_str()),
                    Some("bad_request")
                );
            }
            other => panic!("expected a wrapped error response, got {other:?}"),
        }
    }
}
