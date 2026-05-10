use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

use crate::process::{ProcessDir, ProcessState, REMOTE_BASE, unix_timestamp};
use crate::protocol::{ProcessSummary, Response};

use crate::base64_encode;

/// Get process status with self-healing: if status says "running" but process
/// is dead, update to "exited(unknown)".
pub fn status(id: &str) -> Response {
    let pdir = ProcessDir::new(Path::new(REMOTE_BASE), id);
    let state = match pdir.read_status() {
        Ok(s) => s,
        Err(_) => return Response::error(format!("process not found: {id}")),
    };

    // Self-healing: check if process is actually alive
    let state = if matches!(state, ProcessState::Running) {
        if let Ok(Some(pid)) = pdir.read_pid() {
            let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
            if !alive {
                // Also check runner
                let runner_alive = pdir
                    .read_runner_pid()
                    .ok()
                    .flatten()
                    .map(|rp| unsafe { libc::kill(rp as i32, 0) } == 0)
                    .unwrap_or(false);
                if !runner_alive {
                    let _ = fs::write(pdir.status_path(), "exited(unknown)");
                    let _ = fs::write(pdir.ended_path(), unix_timestamp().to_string());
                    ProcessState::ExitedUnknown
                } else {
                    state
                }
            } else {
                state
            }
        } else {
            state
        }
    } else {
        state
    };

    Response::Status {
        id: id.to_string(),
        state: state.to_string(),
        cmd: pdir.read_cmd().unwrap_or_default(),
        started: pdir.read_started().unwrap_or(0),
        ended: pdir.read_ended().unwrap_or(None),
        stdout_size: pdir.stream_size("stdout"),
        stderr_size: pdir.stream_size("stderr"),
    }
}

/// Default read limit: 1 MiB. Prevents unbounded memory usage for large outputs.
const DEFAULT_READ_LIMIT: u64 = 1024 * 1024;

/// Read process output (stdout or stderr) with optional byte offset and limit.
pub fn read_output(id: &str, stream: &str, offset: Option<u64>, limit: Option<u64>) -> Response {
    let pdir = ProcessDir::new(Path::new(REMOTE_BASE), id);
    let path = match stream {
        "stdout" => pdir.stdout_path(),
        "stderr" => pdir.stderr_path(),
        _ => return Response::error(format!("invalid stream: {stream}")),
    };

    let file_size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    let mut file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return Response::error(format!("process not found: {id}")),
    };

    let offset = offset.unwrap_or(0);
    if offset > 0 {
        if let Err(e) = file.seek(SeekFrom::Start(offset)) {
            return Response::error(format!("seek failed: {e}"));
        }
    }

    let limit = limit.unwrap_or(DEFAULT_READ_LIMIT);
    let mut data = vec![0u8; limit.min(file_size.saturating_sub(offset)) as usize];
    let bytes_read = match file.read(&mut data) {
        Ok(n) => n,
        Err(e) => return Response::error(format!("read failed: {e}")),
    };
    data.truncate(bytes_read);

    Response::Output {
        data: base64_encode(&data),
        offset,
        size: file_size,
    }
}

/// Get the byte size of a stream file.
pub fn size(id: &str, stream: &str) -> Response {
    let pdir = ProcessDir::new(Path::new(REMOTE_BASE), id);
    let sz = pdir.stream_size(stream);
    // Reuse Output response with empty data
    Response::Output {
        data: String::new(),
        offset: 0,
        size: sz,
    }
}

/// Write input to the process's stdin FIFO.
///
/// If `raw` is false (default), a newline is appended. If `raw` is true, the
/// input is sent as-is. Uses O_NONBLOCK to avoid hanging if the process has
/// already exited.
pub fn write_stdin(id: &str, input: &str, raw: bool) -> Response {
    let pdir = ProcessDir::new(Path::new(REMOTE_BASE), id);
    let fifo = pdir.stdin_pipe_path();

    // Check process exists
    if !pdir.dir.exists() {
        return Response::error(format!("process not found: {id}"));
    }

    // Check process is still running
    if let Ok(state) = pdir.read_status() {
        if !matches!(state, ProcessState::Running) {
            return Response::error(format!("process already exited: {id}"));
        }
    }

    // Open FIFO with O_NONBLOCK to avoid hanging if no reader
    let fifo_cstr = match std::ffi::CString::new(fifo.to_str().unwrap_or("")) {
        Ok(c) => c,
        Err(e) => return Response::error(format!("invalid path: {e}")),
    };
    let fd = unsafe { libc::open(fifo_cstr.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        return Response::error(format!("failed to open stdin pipe: {err}"));
    }

    let data = if raw {
        input.as_bytes().to_vec()
    } else {
        format!("{input}\n").into_bytes()
    };

    let written = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
    unsafe { libc::close(fd) };

    if written < 0 {
        let err = std::io::Error::last_os_error();
        Response::error(format!("write failed: {err}"))
    } else {
        Response::Written {
            bytes: written as usize,
        }
    }
}

/// Kill a process by sending SIGTERM to the process group.
///
/// After setsid(), the runner is the process group leader (PGID == runner_pid).
/// The command inherits that PGID. So we kill -runner_pid to hit the entire
/// group (runner + command + any children that haven't changed their PGID).
pub fn kill(id: &str) -> Response {
    let pdir = ProcessDir::new(Path::new(REMOTE_BASE), id);

    if !pdir.dir.exists() {
        return Response::error(format!("process not found: {id}"));
    }

    // Kill the entire process group via the runner (session/group leader)
    if let Ok(Some(rpid)) = pdir.read_runner_pid() {
        unsafe { libc::kill(-(rpid as i32), libc::SIGTERM) };
    } else if let Ok(Some(pid)) = pdir.read_pid() {
        // Fallback: kill the command directly
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }

    let _ = fs::write(pdir.status_path(), "exited(killed)");
    let _ = fs::write(pdir.ended_path(), unix_timestamp().to_string());

    Response::Killed { id: id.to_string() }
}

/// List all managed processes.
pub fn list() -> Response {
    let base = Path::new(REMOTE_BASE);
    let mut processes = Vec::new();

    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            let pdir = ProcessDir::new(base, &id);
            let state = pdir
                .read_status()
                .map(|s| s.to_string())
                .unwrap_or_else(|_| "unknown".to_string());
            let cmd = pdir.read_cmd().unwrap_or_default();
            processes.push(ProcessSummary { id, state, cmd });
        }
    }

    Response::List { processes }
}

/// Clean up exited processes. Returns the list of removed IDs.
pub fn clean() -> Response {
    let base = Path::new(REMOTE_BASE);
    let mut removed = Vec::new();

    if let Ok(entries) = fs::read_dir(base) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            let pdir = ProcessDir::new(base, &id);

            if let Ok(state) = pdir.read_status() {
                if !matches!(state, ProcessState::Running) {
                    // Kill runner if still alive
                    if let Ok(Some(rpid)) = pdir.read_runner_pid() {
                        unsafe { libc::kill(rpid as i32, libc::SIGTERM) };
                    }
                    let _ = fs::remove_dir_all(&pdir.dir);
                    removed.push(id);
                }
            }
        }
    }

    Response::Cleaned { removed }
}

/// Follow (tail) a stream file, writing raw bytes to stdout.
/// This is used by the local daemon's streaming threads.
/// Streams until the process exits and all output is flushed.
/// If `offset` is provided, seeks to that position first (for resume after disconnect).
pub fn follow(id: &str, stream: &str, offset: Option<u64>) -> Response {
    let pdir = ProcessDir::new(Path::new(REMOTE_BASE), id);
    let path = match stream {
        "stdout" => pdir.stdout_path(),
        "stderr" => pdir.stderr_path(),
        _ => return Response::error(format!("invalid stream: {stream}")),
    };

    let mut file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return Response::error(format!("process not found: {id}")),
    };

    // Seek to offset for resume after disconnect
    if let Some(off) = offset {
        if off > 0 {
            let _ = file.seek(SeekFrom::Start(off));
        }
    }

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let mut buf = [0u8; 8192];

    loop {
        match file.read(&mut buf) {
            Ok(0) => {
                // No data available. Check if process has exited.
                if let Ok(state) = pdir.read_status() {
                    if !matches!(state, ProcessState::Running) {
                        // One final read to catch any remaining data
                        match file.read(&mut buf) {
                            Ok(n) if n > 0 => {
                                let _ = stdout.write_all(&buf[..n]);
                                let _ = stdout.flush();
                            }
                            _ => {}
                        }
                        break;
                    }
                }
                // Still running, sleep briefly and retry
                thread::sleep(Duration::from_millis(100));
            }
            Ok(n) => {
                let _ = stdout.write_all(&buf[..n]);
                let _ = stdout.flush();
            }
            Err(_) => break,
        }
    }

    // Don't return a JSON response — follow streams raw bytes.
    // Return a dummy that the caller won't use.
    Response::error("follow completed")
}
