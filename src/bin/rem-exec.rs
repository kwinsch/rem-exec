use std::process::ExitCode;

use clap::{Parser, Subcommand};

use rem_exec::daemon;
use rem_exec::daemon::server;
use rem_exec::protocol::{DaemonRequest, DaemonResponse, Response};
use rem_exec::ssh::{RemoteArgs, ssh_exec_auto_deploy};

#[derive(Parser)]
#[command(name = "rem-exec")]
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
    /// Write to process stdin
    Write {
        /// Remote host
        host: String,
        /// Process ID
        id: String,
        /// Text to send (newline appended unless --raw)
        input: String,
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

/// Route the command through the daemon.
fn route_via_daemon(command: &Command) -> ExitCode {
    let request = match command {
        Command::Start { host, command } => DaemonRequest::Start {
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
        } => DaemonRequest::Write {
            host: host.clone(),
            id: id.clone(),
            input: input.clone(),
            raw: *raw,
        },
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
    let result = match command {
        Command::Start { host, command } => {
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
