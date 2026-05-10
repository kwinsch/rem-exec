use std::process::ExitCode;

use clap::{Parser, Subcommand};

use rem_exec::daemon;
use rem_exec::daemon::server;
use rem_exec::protocol::{DaemonRequest, DaemonResponse, Response};
use rem_exec::ssh::{RemoteArgs, ssh_exec};

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
    },
    /// Write to process stdin
    Write {
        /// Remote host
        host: String,
        /// Process ID
        id: String,
        /// Text to send (newline appended automatically)
        input: String,
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
        Command::Stdout { host, id, offset } => DaemonRequest::Stdout {
            host: host.clone(),
            id: id.clone(),
            offset: *offset,
        },
        Command::Stderr { host, id, offset } => DaemonRequest::Stderr {
            host: host.clone(),
            id: id.clone(),
            offset: *offset,
        },
        Command::Write { host, id, input } => DaemonRequest::Write {
            host: host.clone(),
            id: id.clone(),
            input: input.clone(),
        },
        Command::Kill { host, id } => DaemonRequest::Kill {
            host: host.clone(),
            id: id.clone(),
        },
        Command::List { host } => DaemonRequest::List {
            host: host.clone(),
        },
        Command::Clean { host } => DaemonRequest::Clean {
            host: host.clone(),
        },
        Command::Daemon { .. } => unreachable!(),
    };

    match daemon::send_request(&request) {
        Ok(resp) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&resp).unwrap_or_default()
            );
            match resp {
                DaemonResponse::Error { .. } => ExitCode::FAILURE,
                _ => ExitCode::SUCCESS,
            }
        }
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
            ssh_exec(host, &args.as_str_slice())
        }
        Command::Status { host, id } => {
            let args = RemoteArgs::status(id);
            ssh_exec(host, &args.as_str_slice())
        }
        Command::Stdout { host, id, offset } => {
            let args = RemoteArgs::read(id, "stdout", *offset);
            ssh_exec(host, &args.as_str_slice())
        }
        Command::Stderr { host, id, offset } => {
            let args = RemoteArgs::read(id, "stderr", *offset);
            ssh_exec(host, &args.as_str_slice())
        }
        Command::Write { host, id, input } => {
            let args = RemoteArgs::write(id, input);
            ssh_exec(host, &args.as_str_slice())
        }
        Command::Kill { host, id } => {
            let args = RemoteArgs::kill(id);
            ssh_exec(host, &args.as_str_slice())
        }
        Command::List { host } => {
            let args = RemoteArgs::list();
            ssh_exec(host, &args.as_str_slice())
        }
        Command::Clean { host } => {
            let args = RemoteArgs::clean();
            ssh_exec(host, &args.as_str_slice())
        }
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
