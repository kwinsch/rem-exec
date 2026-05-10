use std::process::ExitCode;

use clap::{Parser, Subcommand};

use rem_exec::process::remote_base;
use rem_exec::protocol::Response;
use rem_exec::remote::{actions, start};

#[derive(Parser)]
#[command(name = "rxd")]
#[command(version)]
#[command(about = "Remote process execution daemon (runs on target host)")]
struct Cli {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Start a new process
    Start {
        /// Command and arguments
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Get process status
    Status {
        /// Process ID
        id: String,
    },
    /// Read process output
    Read {
        /// Process ID
        id: String,
        /// Stream: stdout or stderr
        stream: String,
        /// Byte offset for incremental reads
        #[arg(long)]
        offset: Option<u64>,
        /// Max bytes to read (default: 1 MiB)
        #[arg(long)]
        limit: Option<u64>,
    },
    /// Get output file size
    Size {
        /// Process ID
        id: String,
        /// Stream: stdout or stderr
        #[arg(default_value = "stdout")]
        stream: String,
    },
    /// Write to process stdin
    Write {
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
        /// Process ID
        id: String,
    },
    /// Kill a process
    Kill {
        /// Process ID
        id: String,
    },
    /// List all processes
    List,
    /// Clean up exited processes
    Clean,
    /// Print version and protocol information
    Version,
    /// Pipe stdin to a process (streams raw bytes from SSH channel to FIFO)
    PipeStdin {
        /// Process ID
        id: String,
        /// Keep stdin open after pipe completes (don't kill holder)
        #[arg(long)]
        no_close: bool,
    },
    /// Follow a stream (streams raw bytes, used by daemon)
    Follow {
        /// Process ID
        id: String,
        /// Stream: stdout or stderr
        #[arg(default_value = "stdout")]
        stream: String,
        /// Byte offset to resume from (for reconnect)
        #[arg(long)]
        offset: Option<u64>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // Version needs no state dir — must work on a fresh host before first deploy.
    if matches!(cli.action, Action::Version) {
        let resp = Response::Version {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: rem_exec::protocol::PROTOCOL_VERSION,
        };
        println!("{}", serde_json::to_string(&resp).unwrap());
        return ExitCode::SUCCESS;
    }

    let base = remote_base();
    if let Err(e) = rem_exec::process::ensure_base_dir(&base) {
        let resp = Response::error(e.to_string());
        println!("{}", serde_json::to_string(&resp).unwrap());
        return ExitCode::FAILURE;
    }

    let response = match cli.action {
        Action::Start { command } => match start::start(&command) {
            Ok(r) => r,
            Err(e) => Response::error(e.to_string()),
        },
        Action::Status { id } => actions::status(&id),
        Action::Read {
            id,
            stream,
            offset,
            limit,
        } => actions::read_output(&id, &stream, offset, limit),
        Action::Size { id, stream } => actions::size(&id, &stream),
        Action::Write { id, input, raw } => actions::write_stdin(&id, &input, raw),
        Action::CloseStdin { id } => actions::close_stdin(&id),
        Action::Kill { id } => actions::kill(&id),
        Action::List => actions::list(),
        Action::Clean => actions::clean(),
        Action::PipeStdin { id, no_close } => {
            actions::pipe_stdin(&id, no_close);
            return ExitCode::SUCCESS;
        }
        Action::Follow { id, stream, offset } => {
            actions::follow(&id, &stream, offset);
            return ExitCode::SUCCESS;
        }
        Action::Version => unreachable!("handled above"),
    };

    println!(
        "{}",
        serde_json::to_string(&response).unwrap_or_else(|e| {
            format!("{{\"type\":\"error\",\"message\":\"JSON serialization failed: {e}\"}}")
        })
    );

    ExitCode::SUCCESS
}
