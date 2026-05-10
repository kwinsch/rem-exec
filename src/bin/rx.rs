use std::io::IsTerminal;
use std::process::{ExitCode, Stdio};
use std::thread;

use clap::{Parser, Subcommand};

use rem_exec::daemon;
use rem_exec::daemon::server;
use rem_exec::protocol::{DaemonRequest, DaemonResponse, Response};
use rem_exec::ssh::{RemoteArgs, ssh_exec_auto_deploy, ssh_spawn_piped_stdin};

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
    /// Start a process on a remote host
    Start {
        /// Remote host (SSH destination)
        host: String,
        /// Command to execute
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
        /// Don't close remote stdin after local stdin EOF
        #[arg(long)]
        no_close_stdin: bool,
        /// Bidirectional pipe: stdin→remote stdin, remote stdout→local stdout
        #[arg(long)]
        pipe: bool,
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
            DaemonAction::Start => match server::start_daemon() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            },
            DaemonAction::Stop => match server::stop_daemon() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            },
            DaemonAction::Status => match server::daemon_status() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            },
        };
    }

    // Deploy is always handled locally (no daemon routing)
    if let Command::Deploy { host } = &cli.command {
        return match rem_exec::deploy::deploy_to_host(host) {
            Ok(result) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "type": "deployed",
                        "host": result.host,
                        "arch": result.arch,
                        "version": result.version,
                    }))
                    .unwrap()
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // For all other commands: try daemon first, fall back to direct SSH
    let use_daemon = daemon::is_running();

    if use_daemon {
        route_via_daemon(&cli.command)
    } else {
        route_via_ssh(&cli.command)
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
        _ => ExitCode::SUCCESS, // pipe-stdin exits 1 on EPIPE, which is normal for short-lived commands
    }
}

/// Bidirectional pipe mode: stdin→remote stdin, remote stdout→local stdout.
/// JSON response goes to stderr so stdout carries only data.
fn run_pipe_mode(host: &str, id: &str, response_data: &serde_json::Value) -> ExitCode {
    eprintln!(
        "{}",
        serde_json::to_string(response_data).unwrap_or_default()
    );

    let host_stdin = host.to_string();
    let id_stdin = id.to_string();

    // Thread 1: local stdin → remote stdin
    let stdin_thread = thread::spawn(move || {
        pipe_local_stdin_to_remote(&host_stdin, &id_stdin, false);
    });

    // Main thread: remote stdout → local stdout
    let follow_args = RemoteArgs::follow(id);
    let follow = std::process::Command::new("ssh")
        .arg(host)
        .arg(".local/bin/rxd")
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

/// Route the command through the daemon.
fn route_via_daemon(command: &Command) -> ExitCode {
    // Start with piped stdin or --pipe: send Start to daemon, then pipe stdin directly via SSH
    if let Command::Start {
        host,
        command: cmd,
        no_close_stdin,
        pipe,
    } = command
    {
        let needs_pipe = *pipe || !std::io::stdin().is_terminal();
        if needs_pipe {
            let request = DaemonRequest::Start {
                host: host.clone(),
                command: cmd.clone(),
            };
            return match daemon::send_request(&request) {
                Ok(DaemonResponse::Ok { data }) => {
                    let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    if id.is_empty() {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&data).unwrap_or_default()
                        );
                        return ExitCode::FAILURE;
                    }
                    if *pipe {
                        return run_pipe_mode(host, id, &data);
                    }
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&data).unwrap_or_default()
                    );
                    pipe_local_stdin_to_remote(host, id, *no_close_stdin)
                }
                Ok(DaemonResponse::Error { message }) => {
                    let err = Response::error(message);
                    println!("{}", serde_json::to_string_pretty(&err).unwrap_or_default());
                    ExitCode::FAILURE
                }
                Err(e) => {
                    eprintln!("daemon error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
    }

    let request = match command {
        Command::Start { host, command, .. } => DaemonRequest::Start {
            host: host.clone(),
            command: command.clone(),
        },
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
            // Piped stdin bypasses daemon — streaming is incompatible with request/response
            if input.is_none() && !std::io::stdin().is_terminal() {
                return pipe_local_stdin_to_remote(host, id, true);
            }
            let input = match input {
                Some(s) => s.clone(),
                None => {
                    eprintln!("error: no input provided and stdin is not piped");
                    return ExitCode::FAILURE;
                }
            };
            DaemonRequest::Write {
                host: host.clone(),
                id: id.clone(),
                input,
                raw: *raw,
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
        Command::Deploy { host } => DaemonRequest::Deploy { host: host.clone() },
        Command::Daemon { .. } => unreachable!(),
    };

    match daemon::send_request(&request) {
        Ok(resp) => match resp {
            // Unwrap the DaemonResponse envelope so the CLI output matches
            // direct SSH mode (agent sees identical JSON regardless of daemon state)
            DaemonResponse::Ok { data } => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&data).unwrap_or_default()
                );
                // Check if the inner response is an error
                if data.get("type").and_then(|v| v.as_str()) == Some("error") {
                    ExitCode::FAILURE
                } else {
                    ExitCode::SUCCESS
                }
            }
            DaemonResponse::Error { message } => {
                let err = Response::error(message);
                println!("{}", serde_json::to_string_pretty(&err).unwrap_or_default());
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("daemon error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Route the command directly via SSH (no daemon).
fn route_via_ssh(command: &Command) -> ExitCode {
    // Start with piped stdin or --pipe: start process, then pipe
    if let Command::Start {
        host,
        command: cmd,
        no_close_stdin,
        pipe,
    } = command
    {
        let needs_pipe = *pipe || !std::io::stdin().is_terminal();
        if needs_pipe {
            let args = RemoteArgs::start(cmd);
            let response = match ssh_exec_auto_deploy(host, &args.as_str_slice()) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            if let Response::Started { ref id } = response {
                if *pipe {
                    let data = serde_json::to_value(&response).unwrap_or_default();
                    return run_pipe_mode(host, id, &data);
                }
                println!(
                    "{}",
                    serde_json::to_string_pretty(&response).unwrap_or_default()
                );
                return pipe_local_stdin_to_remote(host, id, *no_close_stdin);
            }
            // Not a Started response (error)
            println!(
                "{}",
                serde_json::to_string_pretty(&response).unwrap_or_default()
            );
            return ExitCode::FAILURE;
        }
    }

    let result = match command {
        Command::Start { host, command, .. } => {
            let args = RemoteArgs::start(command);
            ssh_exec_auto_deploy(host, &args.as_str_slice())
        }
        Command::Status { host, id } => {
            let args = RemoteArgs::status(id);
            ssh_exec_auto_deploy(host, &args.as_str_slice())
        }
        Command::Stdout {
            host,
            id,
            offset,
            limit,
        } => {
            let args = RemoteArgs::read(id, "stdout", *offset, *limit);
            ssh_exec_auto_deploy(host, &args.as_str_slice())
        }
        Command::Stderr {
            host,
            id,
            offset,
            limit,
        } => {
            let args = RemoteArgs::read(id, "stderr", *offset, *limit);
            ssh_exec_auto_deploy(host, &args.as_str_slice())
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
            let args = RemoteArgs::write(id, input, *raw);
            ssh_exec_auto_deploy(host, &args.as_str_slice())
        }
        Command::CloseStdin { host, id } => {
            let args = RemoteArgs::close_stdin(id);
            ssh_exec_auto_deploy(host, &args.as_str_slice())
        }
        Command::Kill { host, id } => {
            let args = RemoteArgs::kill(id);
            ssh_exec_auto_deploy(host, &args.as_str_slice())
        }
        Command::List { host } => {
            let args = RemoteArgs::list();
            ssh_exec_auto_deploy(host, &args.as_str_slice())
        }
        Command::Clean { host } => {
            let args = RemoteArgs::clean();
            ssh_exec_auto_deploy(host, &args.as_str_slice())
        }
        Command::Deploy { .. } => unreachable!("handled above"),
        Command::Daemon { .. } => unreachable!(),
    };

    match result {
        Ok(response) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&response).unwrap_or_default(),
            );
            if matches!(response, Response::Error { .. }) {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
