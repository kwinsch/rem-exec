use std::process::ExitCode;

use clap::{Parser, Subcommand};

use rem_exec::protocol::Response;
use rem_exec::remote::{actions, serve};

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
    /// Handle one framed JSON request from stdin (the primary transport).
    ///
    /// Request = one JSON line, then optional raw body bytes. Response = one
    /// JSON line on stdout. Invoked by `rx` over SSH; not meant to be typed.
    Serve,
    /// Print version and protocol information (bootstrap handshake).
    Version,
    /// Pipe stdin to a process's stdin — raw byte channel (used by rx).
    PipeStdin {
        /// Process ID
        id: String,
        /// Keep stdin open after pipe completes (don't kill holder)
        #[arg(long)]
        no_close: bool,
    },
    /// Follow a stream — raw byte channel (used by rx).
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

    match cli.action {
        Action::Version => {
            let resp = Response::Version {
                version: env!("CARGO_PKG_VERSION").to_string(),
                protocol: rem_exec::protocol::PROTOCOL_VERSION,
            };
            println!("{}", serde_json::to_string(&resp).unwrap());
            ExitCode::SUCCESS
        }
        Action::Serve => serve::serve(),
        Action::PipeStdin { id, no_close } => {
            actions::pipe_stdin(&id, no_close);
            ExitCode::SUCCESS
        }
        Action::Follow { id, stream, offset } => {
            actions::follow(&id, &stream, offset);
            ExitCode::SUCCESS
        }
    }
}
