use std::fs;
use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use rem_exec::process::REMOTE_BASE;
use rem_exec::protocol::Response;
use rem_exec::remote::{actions, start};

#[derive(Parser)]
#[command(name = "rem-execd")]
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
        /// Text to send (newline appended automatically)
        input: String,
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
    /// Follow a stream (streams raw bytes, used by daemon)
    Follow {
        /// Process ID
        id: String,
        /// Stream: stdout or stderr
        #[arg(default_value = "stdout")]
        stream: String,
    },
}

fn main() -> ExitCode {
    // Ensure base directory exists
    let _ = fs::create_dir_all(Path::new(REMOTE_BASE));

    let cli = Cli::parse();

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
        } => actions::read_output(&id, &stream, offset),
        Action::Size { id, stream } => actions::size(&id, &stream),
        Action::Write { id, input } => actions::write_stdin(&id, &input),
        Action::Kill { id } => actions::kill(&id),
        Action::List => actions::list(),
        Action::Clean => actions::clean(),
        Action::Follow { id, stream } => {
            // Follow streams raw bytes, not JSON.
            actions::follow(&id, &stream);
            return ExitCode::SUCCESS;
        }
    };

    // All actions (except follow) output a single JSON line
    println!(
        "{}",
        serde_json::to_string(&response).unwrap_or_else(|e| {
            format!("{{\"type\":\"error\",\"message\":\"JSON serialization failed: {e}\"}}")
        })
    );

    ExitCode::SUCCESS
}
