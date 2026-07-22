use std::collections::BTreeMap;
use std::io::{IsTerminal, Read};
use std::process::{ExitCode, Stdio};
use std::thread;

use clap::{Parser, Subcommand};

use rem_exec::daemon;
use rem_exec::daemon::server;
use rem_exec::protocol::{DaemonRequest, DaemonResponse, Request, Response};
use rem_exec::ssh::{
    REMOTE_BIN, RemoteArgs, serve_request_auto_deploy, ssh_command, ssh_spawn_piped_stdin,
};

/// Cap on stdin inlined into a `run`/`write` request. Larger inputs should use
/// the streaming path (`rx start --pipe`), which has no size limit.
const INLINE_STDIN_CAP: usize = 4 * 1024 * 1024;

#[derive(Parser)]
#[command(name = "rx")]
#[command(version)]
#[command(about = "Agent-friendly remote process execution")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the local daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Run a command to completion (blocks up to --timeout, then backgrounds)
    Run {
        /// Remote host (SSH destination)
        host: String,
        /// Command to execute
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
        /// Working directory for the command
        #[arg(long)]
        cwd: Option<String>,
        /// Environment override (repeatable): --env KEY=VALUE
        #[arg(long = "env", value_name = "K=V")]
        env: Vec<String>,
        /// Seconds to wait before backgrounding the process (default 30)
        #[arg(long)]
        timeout: Option<u64>,
        /// Keep stdin open instead of sending EOF after any piped input
        #[arg(long)]
        keep_stdin_open: bool,
    },
    /// Start a detached process on a remote host
    Start {
        /// Remote host (SSH destination)
        host: String,
        /// Command to execute
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
        /// Working directory for the command
        #[arg(long)]
        cwd: Option<String>,
        /// Environment override (repeatable): --env KEY=VALUE
        #[arg(long = "env", value_name = "K=V")]
        env: Vec<String>,
        /// Don't close remote stdin after local stdin EOF
        #[arg(long)]
        no_close_stdin: bool,
        /// Bidirectional pipe: stdin→remote stdin, remote stdout→local stdout
        #[arg(long)]
        pipe: bool,
    },
    /// Wait for a process to exit (blocks up to --timeout, then returns a handle)
    Wait {
        /// Remote host
        host: String,
        /// Process ID
        id: String,
        /// Seconds to wait before returning a running handle (default 30)
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Get process status
    Status {
        /// Remote host
        host: String,
        /// Process ID
        id: String,
    },
    /// Read stdout
    Stdout {
        /// Remote host
        host: String,
        /// Process ID
        id: String,
        /// Byte offset for incremental reads
        #[arg(long)]
        offset: Option<u64>,
        /// Max bytes to read (default: 1 MiB)
        #[arg(long)]
        limit: Option<u64>,
    },
    /// Read stderr
    Stderr {
        /// Remote host
        host: String,
        /// Process ID
        id: String,
        /// Byte offset for incremental reads
        #[arg(long)]
        offset: Option<u64>,
        /// Max bytes to read (default: 1 MiB)
        #[arg(long)]
        limit: Option<u64>,
    },
    /// Write to process stdin (if input omitted, reads from piped stdin)
    Write {
        /// Remote host
        host: String,
        /// Process ID
        id: String,
        /// Text to send (newline appended unless --raw). If omitted, reads from stdin.
        input: Option<String>,
        /// Send input without appending a newline
        #[arg(long)]
        raw: bool,
    },
    /// Close stdin (send EOF to the process)
    CloseStdin {
        /// Remote host
        host: String,
        /// Process ID
        id: String,
    },
    /// Kill a process
    Kill {
        /// Remote host
        host: String,
        /// Process ID
        id: String,
    },
    /// List all processes on a host
    List {
        /// Remote host
        host: String,
    },
    /// Clean up exited processes
    Clean {
        /// Remote host
        host: String,
    },
    /// Deploy rem-execd to a remote host (detects architecture automatically)
    Deploy {
        /// Remote host (SSH destination)
        host: String,
    },
    /// Copy a local file to a remote path (atomic; optional mode/owner/group)
    Cp {
        /// Local source file
        local: String,
        /// Destination as HOST:REMOTE_PATH
        remote: String,
        /// File mode in octal, e.g. 0644
        #[arg(long)]
        mode: Option<String>,
        /// Owner user name or uid (needs a privileged rxd)
        #[arg(long)]
        owner: Option<String>,
        /// Group name or gid (needs a privileged rxd)
        #[arg(long)]
        group: Option<String>,
    },
    /// Download static rxd binaries into the local deploy cache
    Setup {
        /// Release tag or version (default: current rx version)
        #[arg(long)]
        version: Option<String>,
        /// Architecture to cache (repeatable; default: all supported)
        #[arg(long)]
        arch: Vec<String>,
        /// Re-download even if the cached binary already matches SHA256SUMS
        #[arg(long)]
        force: bool,
    },
    /// Print skill file (machine-readable usage guide)
    Skill,
}

#[derive(Subcommand)]
enum DaemonAction {
    /// Start the daemon
    Start,
    /// Stop the daemon
    Stop,
    /// Show daemon status
    Status,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Handle daemon subcommands directly
    if let Command::Daemon { action } = &cli.command {
        return match action {
            DaemonAction::Start => report(server::start_daemon()),
            DaemonAction::Stop => report(server::stop_daemon()),
            DaemonAction::Status => report(server::daemon_status()),
        };
    }

    // Deploy is always handled locally (no daemon routing)
    if let Command::Deploy { host } = &cli.command {
        return match rem_exec::deploy::deploy_to_host(host) {
            Ok(result) => {
                print_json(&serde_json::json!({
                    "type": "deployed",
                    "host": result.host,
                    "arch": result.arch,
                    "version": result.version,
                }));
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // Copy streams a file directly over SSH (not through the daemon).
    if let Command::Cp {
        local,
        remote,
        mode,
        owner,
        group,
    } = &cli.command
    {
        return do_cp(local, remote, mode.as_deref(), owner.clone(), group.clone());
    }

    // Setup is always local: it populates the rxd deploy cache.
    if let Command::Setup {
        version,
        arch,
        force,
    } = &cli.command
    {
        return match rem_exec::deploy::setup_release_binaries(version.as_deref(), arch, *force) {
            Ok(result) => {
                let binaries: Vec<_> = result
                    .binaries
                    .iter()
                    .map(|binary| {
                        let status = match binary.status {
                            rem_exec::deploy::SetupStatus::Cached => "cached",
                            rem_exec::deploy::SetupStatus::Installed => "installed",
                        };
                        serde_json::json!({
                            "arch": binary.arch,
                            "path": binary.path.display().to_string(),
                            "sha256": binary.sha256,
                            "status": status,
                        })
                    })
                    .collect();
                print_json(&serde_json::json!({
                    "type": "setup",
                    "version": result.version,
                    "binaries": binaries,
                }));
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    if matches!(cli.command, Command::Skill) {
        print!("{}", include_str!("../../docs/llm.txt"));
        return ExitCode::SUCCESS;
    }

    // For all other commands: try daemon first, fall back to direct SSH
    if daemon::is_running() {
        route_via_daemon(&cli.command)
    } else {
        route_via_ssh(&cli.command)
    }
}

/// Read piped local stdin up to `INLINE_STDIN_CAP`. Returns None if stdin is a
/// terminal (no piped input). Errors if input exceeds the cap.
fn read_inline_stdin() -> Result<Option<Vec<u8>>, String> {
    if std::io::stdin().is_terminal() {
        return Ok(None);
    }
    let mut buf = Vec::new();
    let mut handle = std::io::stdin().lock().take((INLINE_STDIN_CAP + 1) as u64);
    handle.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if buf.len() > INLINE_STDIN_CAP {
        return Err(format!(
            "stdin exceeds inline cap ({INLINE_STDIN_CAP} bytes); use `rx start --pipe` for large input"
        ));
    }
    Ok(Some(buf))
}

/// Bytes to write for an inline `write` (newline appended unless raw).
fn write_bytes(input: &str, raw: bool) -> Vec<u8> {
    if raw {
        input.as_bytes().to_vec()
    } else {
        format!("{input}\n").into_bytes()
    }
}

/// Pipe local stdin to a remote process via SSH pipe-stdin.
fn pipe_local_stdin_to_remote(host: &str, id: &str, no_close: bool) -> ExitCode {
    let args = RemoteArgs::pipe_stdin(id, no_close);
    let mut child = match ssh_spawn_piped_stdin(host, &args.as_str_slice()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to spawn pipe-stdin: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut remote_stdin = child.stdin.take().unwrap();
    let mut local_stdin = std::io::stdin().lock();
    let _ = std::io::copy(&mut local_stdin, &mut remote_stdin);
    drop(remote_stdin);

    match child.wait() {
        Ok(s) if s.success() => ExitCode::SUCCESS,
        _ => ExitCode::SUCCESS, // pipe-stdin exits 1 on EPIPE, normal for short-lived commands
    }
}

/// Bidirectional pipe mode: stdin→remote stdin, remote stdout→local stdout.
/// JSON response goes to stderr so stdout carries only data.
fn run_pipe_mode(host: &str, id: &str, response_data: &serde_json::Value) -> ExitCode {
    eprintln!("{}", serde_json::to_string(response_data).unwrap_or_default());

    let host_stdin = host.to_string();
    let id_stdin = id.to_string();

    // Thread 1: local stdin → remote stdin
    let stdin_thread = thread::spawn(move || {
        pipe_local_stdin_to_remote(&host_stdin, &id_stdin, false);
    });

    // Main thread: remote stdout → local stdout
    let follow_args = RemoteArgs::follow(id);
    let follow = ssh_command(host)
        .arg(REMOTE_BIN)
        .args(follow_args.as_str_slice())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();

    if let Ok(mut child) = follow {
        if let Some(mut remote_stdout) = child.stdout.take() {
            let _ = std::io::copy(&mut remote_stdout, &mut std::io::stdout());
        }
        let _ = child.wait();
    }

    let _ = stdin_thread.join();
    ExitCode::SUCCESS
}

/// Route the command directly via SSH (no daemon).
fn route_via_ssh(command: &Command) -> ExitCode {
    match command {
        Command::Run {
            host,
            command: cmd,
            cwd,
            env,
            timeout,
            keep_stdin_open,
        } => {
            let body = match read_inline_stdin() {
                Ok(b) => b.unwrap_or_default(),
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let env = match parse_env(env) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let request = Request::Run {
                command: cmd.clone(),
                cwd: cwd.clone(),
                env,
                timeout_ms: timeout.map(|s| s.saturating_mul(1000)),
                keep_stdin_open: *keep_stdin_open,
            };
            dispatch_run(host, &request, &body)
        }

        Command::Wait { host, id, timeout } => dispatch_run(
            host,
            &Request::Wait {
                id: id.clone(),
                timeout_ms: timeout.map(|s| s.saturating_mul(1000)),
            },
            &[],
        ),

        Command::Start {
            host,
            command: cmd,
            cwd,
            env,
            no_close_stdin,
            pipe,
        } => {
            let env = match parse_env(env) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let needs_pipe = *pipe || !std::io::stdin().is_terminal();
            let request = Request::Start {
                command: cmd.clone(),
                cwd: cwd.clone(),
                env,
            };
            let response = match serve_request_auto_deploy(host, &request, &[]) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match &response {
                Response::Started { id } if needs_pipe => {
                    if *pipe {
                        let data = serde_json::to_value(&response).unwrap_or_default();
                        run_pipe_mode(host, id, &data)
                    } else {
                        print_json_response(&response);
                        pipe_local_stdin_to_remote(host, id, *no_close_stdin)
                    }
                }
                _ => {
                    print_json_response(&response);
                    exit_for(&response)
                }
            }
        }

        Command::Write {
            host,
            id,
            input,
            raw,
        } => {
            if input.is_none() && !std::io::stdin().is_terminal() {
                return pipe_local_stdin_to_remote(host, id, true);
            }
            let input = match input {
                Some(s) => s,
                None => {
                    eprintln!("error: no input provided and stdin is not piped");
                    return ExitCode::FAILURE;
                }
            };
            let body = write_bytes(input, *raw);
            let request = Request::Write { id: id.clone() };
            dispatch_simple(host, &request, &body)
        }

        Command::Status { host, id } => {
            dispatch_simple(host, &Request::Status { id: id.clone() }, &[])
        }
        Command::Stdout {
            host,
            id,
            offset,
            limit,
        } => dispatch_simple(
            host,
            &Request::Read {
                id: id.clone(),
                stream: "stdout".to_string(),
                offset: *offset,
                limit: *limit,
            },
            &[],
        ),
        Command::Stderr {
            host,
            id,
            offset,
            limit,
        } => dispatch_simple(
            host,
            &Request::Read {
                id: id.clone(),
                stream: "stderr".to_string(),
                offset: *offset,
                limit: *limit,
            },
            &[],
        ),
        Command::CloseStdin { host, id } => {
            dispatch_simple(host, &Request::CloseStdin { id: id.clone() }, &[])
        }
        Command::Kill { host, id } => dispatch_simple(host, &Request::Kill { id: id.clone() }, &[]),
        Command::List { host } => dispatch_simple(host, &Request::List, &[]),
        Command::Clean { host } => dispatch_simple(host, &Request::Clean, &[]),

        Command::Deploy { .. }
        | Command::Cp { .. }
        | Command::Setup { .. }
        | Command::Skill
        | Command::Daemon { .. } => unreachable!("handled before routing"),
    }
}

/// Send a request over SSH, print the JSON response, and map its type to an exit
/// code.
fn dispatch_simple(host: &str, request: &Request, body: &[u8]) -> ExitCode {
    match serve_request_auto_deploy(host, request, body) {
        Ok(resp) => {
            print_json_response(&resp);
            exit_for(&resp)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Like [`dispatch_simple`] but propagates a completed command's exit status
/// (for `run` / `wait`).
fn dispatch_run(host: &str, request: &Request, body: &[u8]) -> ExitCode {
    match serve_request_auto_deploy(host, request, body) {
        Ok(resp) => {
            print_json_response(&resp);
            run_exit(&resp)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Parse repeated `--env K=V` into a map (later duplicates win).
fn parse_env(pairs: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut map = BTreeMap::new();
    for p in pairs {
        match p.split_once('=') {
            Some((k, v)) if !k.is_empty() => {
                map.insert(k.to_string(), v.to_string());
            }
            _ => return Err(format!("invalid --env '{p}' (expected K=V)")),
        }
    }
    Ok(map)
}

/// Parse an octal file mode like `0644`, `644`, or `0o644`.
fn parse_mode(s: &str) -> Result<u32, String> {
    let digits = s.strip_prefix("0o").unwrap_or(s);
    u32::from_str_radix(digits, 8)
        .map_err(|_| format!("invalid --mode '{s}' (expected octal like 0644)"))
}

/// Stream a local file to `HOST:PATH` via the `put` request, with one
/// auto-deploy retry. Runs direct (not through the daemon).
fn do_cp(
    local: &str,
    remote: &str,
    mode: Option<&str>,
    owner: Option<String>,
    group: Option<String>,
) -> ExitCode {
    let (host, path) = match remote.split_once(':') {
        Some((h, p)) if !h.is_empty() && !p.is_empty() => (h, p),
        _ => {
            eprintln!("error: destination must be HOST:PATH");
            return ExitCode::FAILURE;
        }
    };
    let mode = match mode {
        Some(s) => match parse_mode(s) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    let request = Request::Put {
        path: path.to_string(),
        mode,
        owner,
        group,
    };

    // Each send re-opens the file so an auto-deploy retry starts fresh.
    let send = || -> rem_exec::error::Result<Response> {
        let mut f = std::fs::File::open(local)?;
        rem_exec::ssh::serve_request_stream(host, &request, &mut f)
    };

    let result = match send() {
        Err(e) if rem_exec::deploy::auto_deploy_enabled() && rem_exec::deploy::should_auto_deploy(&e) => {
            match rem_exec::deploy::deploy_to_host(host) {
                Ok(_) => send(),
                Err(de) => Err(rem_exec::error::RemExecError::Ssh(format!(
                    "auto-deploy to {host} failed: {de} (original: {e})"
                ))),
            }
        }
        other => other,
    };

    match result {
        Ok(resp) => {
            print_json_response(&resp);
            exit_for(&resp)
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Route the command through the local daemon.
fn route_via_daemon(command: &Command) -> ExitCode {
    // Start with piped stdin or --pipe: send Start to daemon, then pipe directly
    if let Command::Start {
        host,
        command: cmd,
        cwd,
        env,
        no_close_stdin,
        pipe,
    } = command
    {
        let needs_pipe = *pipe || !std::io::stdin().is_terminal();
        if needs_pipe {
            let env = match parse_env(env) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let request = DaemonRequest::Start {
                host: host.clone(),
                command: cmd.clone(),
                cwd: cwd.clone(),
                env,
            };
            return match daemon::send_request(&request) {
                Ok(DaemonResponse::Ok { data }) => {
                    let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    if id.is_empty() {
                        print_json(&data);
                        return ExitCode::FAILURE;
                    }
                    if *pipe {
                        return run_pipe_mode(host, id, &data);
                    }
                    print_json(&data);
                    pipe_local_stdin_to_remote(host, id, *no_close_stdin)
                }
                Ok(DaemonResponse::Error { message }) => {
                    print_json_response(&Response::error(message));
                    ExitCode::FAILURE
                }
                Err(e) => {
                    eprintln!("daemon error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
    }

    // Run with piped stdin bypasses nothing — the body rides in the request.
    let request = match command {
        Command::Run {
            host,
            command: cmd,
            cwd,
            env,
            timeout,
            keep_stdin_open,
        } => {
            let stdin_b64 = match read_inline_stdin() {
                Ok(Some(b)) if !b.is_empty() => Some(rem_exec::base64_encode(&b)),
                Ok(_) => None,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let env = match parse_env(env) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            DaemonRequest::Run {
                host: host.clone(),
                command: cmd.clone(),
                cwd: cwd.clone(),
                env,
                timeout_ms: timeout.map(|s| s.saturating_mul(1000)),
                stdin_b64,
                keep_stdin_open: *keep_stdin_open,
            }
        }
        Command::Wait { host, id, timeout } => DaemonRequest::Wait {
            host: host.clone(),
            id: id.clone(),
            timeout_ms: timeout.map(|s| s.saturating_mul(1000)),
        },
        Command::Start {
            host,
            command,
            cwd,
            env,
            ..
        } => {
            let env = match parse_env(env) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            DaemonRequest::Start {
                host: host.clone(),
                command: command.clone(),
                cwd: cwd.clone(),
                env,
            }
        }
        Command::Status { host, id } => DaemonRequest::Status {
            host: host.clone(),
            id: id.clone(),
        },
        Command::Stdout {
            host,
            id,
            offset,
            limit,
        } => DaemonRequest::Stdout {
            host: host.clone(),
            id: id.clone(),
            offset: *offset,
            limit: *limit,
        },
        Command::Stderr {
            host,
            id,
            offset,
            limit,
        } => DaemonRequest::Stderr {
            host: host.clone(),
            id: id.clone(),
            offset: *offset,
            limit: *limit,
        },
        Command::Write {
            host,
            id,
            input,
            raw,
        } => {
            if input.is_none() && !std::io::stdin().is_terminal() {
                return pipe_local_stdin_to_remote(host, id, true);
            }
            let input = match input {
                Some(s) => s,
                None => {
                    eprintln!("error: no input provided and stdin is not piped");
                    return ExitCode::FAILURE;
                }
            };
            DaemonRequest::Write {
                host: host.clone(),
                id: id.clone(),
                data_b64: rem_exec::base64_encode(&write_bytes(input, *raw)),
            }
        }
        Command::CloseStdin { host, id } => DaemonRequest::CloseStdin {
            host: host.clone(),
            id: id.clone(),
        },
        Command::Kill { host, id } => DaemonRequest::Kill {
            host: host.clone(),
            id: id.clone(),
        },
        Command::List { host } => DaemonRequest::List { host: host.clone() },
        Command::Clean { host } => DaemonRequest::Clean { host: host.clone() },
        Command::Deploy { .. }
        | Command::Cp { .. }
        | Command::Setup { .. }
        | Command::Skill
        | Command::Daemon { .. } => unreachable!("handled before routing"),
    };

    let is_run = matches!(command, Command::Run { .. } | Command::Wait { .. });
    match daemon::send_request(&request) {
        Ok(DaemonResponse::Ok { data }) => {
            print_json(&data);
            if data.get("type").and_then(|v| v.as_str()) == Some("error") {
                ExitCode::FAILURE
            } else if is_run {
                run_exit_from_value(&data)
            } else {
                ExitCode::SUCCESS
            }
        }
        Ok(DaemonResponse::Error { message }) => {
            print_json_response(&Response::error(message));
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("daemon error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Exit code for a non-run response: FAILURE only for an error response.
fn exit_for(response: &Response) -> ExitCode {
    if matches!(response, Response::Error { .. }) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Exit code for a `run`: propagate the remote command's exit status so
/// `rx run host false` behaves like a normal command runner. Agents still read
/// the structured result from the JSON.
fn run_exit(response: &Response) -> ExitCode {
    match response {
        Response::Completed {
            exit_code, signal, ..
        } => {
            if let Some(code) = exit_code {
                ExitCode::from((*code).clamp(0, 255) as u8)
            } else if let Some(sig) = signal {
                ExitCode::from((128 + *sig).clamp(0, 255) as u8)
            } else {
                ExitCode::SUCCESS
            }
        }
        Response::Error { .. } => ExitCode::FAILURE,
        _ => ExitCode::SUCCESS,
    }
}

fn run_exit_from_value(data: &serde_json::Value) -> ExitCode {
    if data.get("type").and_then(|v| v.as_str()) == Some("completed") {
        if let Some(code) = data.get("exit_code").and_then(|v| v.as_i64()) {
            return ExitCode::from(code.clamp(0, 255) as u8);
        }
        if let Some(sig) = data.get("signal").and_then(|v| v.as_i64()) {
            return ExitCode::from((128 + sig).clamp(0, 255) as u8);
        }
    }
    ExitCode::SUCCESS
}

fn print_json_response(response: &Response) {
    println!(
        "{}",
        serde_json::to_string_pretty(response).unwrap_or_default()
    );
}

fn print_json(value: &serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
}

fn report(result: rem_exec::error::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
