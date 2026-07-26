use std::collections::BTreeMap;
use std::io::{IsTerminal, Read};
use std::process::{ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use clap::{Parser, Subcommand};

use rem_exec::daemon;
use rem_exec::daemon::server;
use rem_exec::error::RemExecError;
use rem_exec::protocol::{DaemonRequest, DaemonResponse, ErrorCode, Request, Response};
use rem_exec::ssh::{
    REMOTE_BIN, RemoteArgs, serve_request_auto_deploy, ssh_command, ssh_spawn_piped_stdin,
};

/// Cap on stdin inlined into a `run`/`write` request. Larger inputs should use
/// the streaming path (`rx start --pipe`), which has no size limit.
const INLINE_STDIN_CAP: usize = 4 * 1024 * 1024;

/// Process-wide switch: emit compact single-line JSON instead of pretty output.
/// Set once at startup from [`compact_requested`].
static COMPACT_JSON: AtomicBool = AtomicBool::new(false);

/// Process-wide switch: this invocation's stdout carries raw bytes, so the JSON
/// object goes to stderr. Set once at startup from [`object_goes_to_stdout`].
///
/// A switch rather than three careful call sites. `start --pipe` failed on
/// stdout from the host check, the transport classifier and the rxd error arm
/// alike, and each was individually plausible — the guarantee only holds if it
/// is structural, so every object this binary prints goes through [`emit_json`].
static OBJECT_TO_STDERR: AtomicBool = AtomicBool::new(false);

/// `RX_JSON`, or the older `REM_EXEC_JSON`, normalized.
fn json_env() -> Option<String> {
    std::env::var("RX_JSON")
        .or_else(|_| std::env::var("REM_EXEC_JSON"))
        .ok()
        .map(|v| v.trim().to_ascii_lowercase())
}

/// Whether to render JSON compact.
///
/// Pretty output is a courtesy to a person reading a terminal; a pipe, a file
/// or an agent harness pays for it in bytes and gets nothing back. So the
/// default follows the destination rather than a flag nobody remembers to pass
/// — and either side can still be forced, because "is stdout a terminal" is not
/// something a caller should have to reason about when it matters.
///
/// Precedence: explicit flag → `RX_JSON`/`REM_EXEC_JSON` → stdout is not a tty.
fn compact_requested(compact: bool, pretty: bool) -> bool {
    if compact {
        return true;
    }
    if pretty {
        return false;
    }
    match json_env().as_deref() {
        Some("compact") => true,
        Some("pretty") => false,
        _ => !std::io::stdout().is_terminal(),
    }
}

#[derive(Parser)]
#[command(name = "rx")]
#[command(version)]
#[command(about = "Agent-friendly remote process execution")]
#[command(after_help = concat!("\
Start here:  rx skill        the full guide — commands, response shapes, error
                             codes. Agent-oriented; pipe it to a pager to read.
First contact with a host:   rx ping HOST  →  if it answers not_deployed,
                             rx deploy HOST  (one static binary, no remote deps)

Every command that does something answers with one JSON object. Exit 0 =
success, 1 = the call failed, 2 = the call was malformed. --help, --version and
skill are discovery: plain text, no object, no side effects.

Placing a secret? Pipe any producer into `put -` — the value never touches a
local file or an argument, and the mode is applied before the file is visible:

  <producer> | rx put - HOST:/run/secrets/name --mode 0600

rxv, pass, op read and sops -d all work; rx needs none of them.

Docs and releases: ", env!("CARGO_PKG_REPOSITORY")))]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Emit single-line JSON (the default when stdout is not a terminal)
    #[arg(long, global = true, conflicts_with = "pretty")]
    compact: bool,
    /// Emit pretty-printed JSON (the default when stdout is a terminal)
    #[arg(long, global = true)]
    pretty: bool,
}

// Declaration order is what `--help` shows. It runs in the order an agent
// meets them: the guide, first contact with a host, then execute, observe,
// control, move files, and finally local machinery. `ping` and `deploy` are
// what the help text tells you to run first, so they are no longer twelfth
// and thirteenth, below `close-stdin`.
#[derive(Subcommand)]
enum Command {
    /// The full guide: commands, response shapes, error codes (agent-oriented,
    /// ~400 lines — pipe it to a pager)
    Skill,
    /// Probe reachability + host identity (rxd version, OS, kernel, arch, distro)
    Ping {
        /// Remote host (SSH destination)
        host: String,
    },
    /// Install the matching rxd on one or more hosts (detects architecture automatically)
    Deploy {
        /// Remote hosts (SSH destinations)
        #[arg(required = true)]
        hosts: Vec<String>,
        /// Deploy this local rxd build instead of a cached release asset
        #[arg(long, value_name = "PATH")]
        binary: Option<std::path::PathBuf>,
        /// Never download; deploy only what the local cache already has
        #[arg(long)]
        offline: bool,
        /// Allow replacing an rxd that is ahead of this rx — by protocol or by version
        #[arg(long)]
        allow_downgrade: bool,
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
        /// Keep process state after a fully-inlined completed run (default:
        /// remove the remote process dir so short runs do not accumulate)
        #[arg(long)]
        keep: bool,
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
    /// Write a local file — or stdin, with `-` — to a remote path (atomic;
    /// optional mode/owner/group)
    #[command(alias = "cp")]
    Put {
        /// Local source file, or `-` to stream stdin (use ./- for a file so named)
        local: String,
        /// Destination as HOST:REMOTE_PATH
        remote: String,
        /// File mode in octal, e.g. 0644 (default: 0600)
        #[arg(long)]
        mode: Option<String>,
        /// Owner user name or uid (needs a privileged rxd)
        #[arg(long)]
        owner: Option<String>,
        /// Group name or gid (needs a privileged rxd)
        #[arg(long)]
        group: Option<String>,
        /// With `-`: write an empty file when stdin carries no bytes, instead of
        /// refusing (an empty stream usually means the producer failed)
        #[arg(long)]
        allow_empty: bool,
    },
    /// Download a remote file to a local path (atomic; verifies full size)
    Get {
        /// Source as HOST:REMOTE_PATH
        remote: String,
        /// Local destination file path
        local: String,
        /// Mode in octal for the local file, e.g. 0644 (default: source's mode)
        #[arg(long)]
        mode: Option<String>,
    },
    /// Manage the local cache of rxd binaries that `deploy` installs from
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Manage the local daemon (an opt-in read cache; direct SSH is the default)
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

/// `cache` is a namespace rather than a single command because pruning is
/// already planned; a leaf `rx cache` would have to become this later, and
/// renaming twice is worse than naming it right once.
#[derive(Subcommand)]
enum CacheAction {
    /// Download static rxd binaries into the local cache
    Fetch {
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
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => return parse_failure(err),
    };

    if compact_requested(cli.compact, cli.pretty) {
        COMPACT_JSON.store(true, Ordering::Relaxed);
    }

    // Fixed before the host check below, which is the first thing that can
    // print — under `--pipe` an object on stdout lands in the consumer's byte
    // stream, which is the failure `docs/CONTRACT.md` exists to prevent.
    if !object_goes_to_stdout(&cli.command) {
        OBJECT_TO_STDERR.store(true, Ordering::Relaxed);
    }

    // The deploy policy is read from RX_AUTO_DEPLOY where it is needed, and
    // defaults to "off" — rx never changes a host you did not point it at.
    // There is deliberately no flag: whether a fleet may self-heal is a property
    // of the harness, not a decision to re-make on every call, and as a flag it
    // was accepted-but-inert on `skill`, `cache` and `daemon`. Same reasoning as
    // RX_CONNECT_TIMEOUT (see `ssh::connect_timeout`).

    // Validate every destination once, before anything is spawned. `--` in
    // ssh_command is what actually blocks option injection; this exists so a
    // bad host is a typed `bad_host` an agent can branch on instead of an
    // OpenSSH message about invalid hostname characters.
    for host in command_hosts(&cli.command) {
        if let Err(e) = rem_exec::ssh::validate_host(host) {
            return fail(rem_exec::protocol::ErrorCode::BadHost, e.to_string());
        }
    }

    // A process ID that is not the 8-hex form can name no process on any host,
    // so answer without a round trip. rxd still checks — an older rx reaching a
    // current rxd must not lose the guard — and both emit the same code, so the
    // answer does not depend on which side caught it.
    //
    // The round trip was not merely slow: against an unreachable host the reply
    // was `ssh_unreachable` with `retryable:true`, so a typo'd ID looked like a
    // transient network problem and an agent would retry it forever.
    if let Some(id) = command_process_id(&cli.command)
        && !rem_exec::process::is_valid_process_id(id)
    {
        return fail(
            rem_exec::protocol::ErrorCode::InvalidProcessId,
            format!("invalid process ID: {id}"),
        );
    }

    // Handle daemon subcommands directly
    if let Command::Daemon { action } = &cli.command {
        return match action {
            DaemonAction::Start => report(server::start_daemon()),
            DaemonAction::Stop => report(server::stop_daemon()),
            DaemonAction::Status => report(server::daemon_status()),
        };
    }

    // Deploy is always handled locally (no daemon routing)
    if let Command::Deploy {
        hosts,
        binary,
        offline,
        allow_downgrade,
    } = &cli.command
    {
        return do_deploy(hosts, binary.clone(), *offline, *allow_downgrade);
    }

    // Put streams its body directly over SSH (not through the daemon).
    if let Command::Put {
        local,
        remote,
        mode,
        owner,
        group,
        allow_empty,
    } = &cli.command
    {
        return do_put(
            local,
            remote,
            mode.as_deref(),
            owner.clone(),
            group.clone(),
            *allow_empty,
        );
    }

    // Ping is a stateless probe: go direct over SSH (no daemon), honoring
    // auto-deploy like other remote commands.
    if let Command::Ping { host } = &cli.command {
        return do_ping(host);
    }

    // Get streams a remote file down directly over SSH (not through the daemon).
    if let Command::Get {
        remote,
        local,
        mode,
    } = &cli.command
    {
        return do_get(remote, local, mode.as_deref());
    }

    // Cache is always local: it populates the rxd binary cache deploy reads.
    if let Command::Cache {
        action:
            CacheAction::Fetch {
                version,
                arch,
                force,
            },
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
                // `action` mirrors the `daemon` namespace, so a namespaced
                // command answers the same way wherever one appears.
                print_json(&serde_json::json!({
                    "type": "cache",
                    "action": "fetch",
                    "version": result.version,
                    "binaries": binaries,
                }));
                ExitCode::SUCCESS
            }
            // A release that answers 404 is not an internal failure and does not
            // appear on a second attempt, so it must not arrive as the one code
            // that means "retry": `--version v9.9.9` would loop forever. Same
            // answer `deploy` already gives for the same condition.
            Err(RemExecError::AssetNotFound(message)) => fail_hint(
                ErrorCode::DeployFailed,
                message,
                "check the tag with `gh release list` (or the releases page); \
                 `--version vX.Y.Z` selects another one",
            ),
            // Unsupported `--arch` is a permanent client mistake, same class as
            // a missing release: never `internal`/`retryable:true`.
            Err(e) => {
                let message = e.to_string();
                if message.starts_with("unsupported architecture") {
                    fail(ErrorCode::BadRequest, message)
                } else {
                    fail(ErrorCode::Internal, message)
                }
            }
        };
    }

    if matches!(cli.command, Command::Skill) {
        // Stamped with the version that shipped it: a guide read from anywhere
        // else can then be recognised as describing a different binary.
        //
        // Written through the same EPIPE-tolerant path as the objects: piping
        // the guide into a pager and quitting early is the normal way to read
        // it, and that must not end in a panic.
        let guide =
            include_str!("../../docs/llm.txt").replace("{{VERSION}}", env!("CARGO_PKG_VERSION"));
        write_all(&mut std::io::stdout(), &guide);
        return ExitCode::SUCCESS;
    }

    // Direct SSH is the canonical path. The local daemon is an optional
    // accelerator for repeated reads of long-running processes; it handles a
    // command only when explicitly opted in (RX_DAEMON=1), so a daemon
    // that merely happens to be running never silently changes how a command is
    // transported or how it fails.
    if daemon_enabled() {
        if daemon::is_running() {
            route_via_daemon(&cli.command)
        } else {
            eprintln!("note: RX_DAEMON set but no daemon running — using direct SSH");
            route_via_ssh(&cli.command)
        }
    } else {
        route_via_ssh(&cli.command)
    }
}

/// Whether the caller opted into routing through the local daemon. Direct SSH
/// is always the default; the daemon is used only when this is set, so a daemon
/// that merely happens to be running never reroutes commands.
fn daemon_enabled() -> bool {
    let value = std::env::var("RX_DAEMON").or_else(|_| std::env::var("REM_EXEC_DAEMON"));
    daemon_opt_in(value.ok().as_deref())
}

/// Pure predicate behind [`daemon_enabled`], split out so it is testable without
/// touching the process environment.
fn daemon_opt_in(value: Option<&str>) -> bool {
    match value {
        Some(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        None => false,
    }
}

/// Why inline stdin could not be used.
///
/// The split is what lets the CLI answer correctly: too much input is the
/// caller's to fix and there is another command that takes it, while a read
/// failure is ours and nothing the caller does differently would help.
enum StdinError {
    /// More than [`INLINE_STDIN_CAP`] bytes were piped in.
    TooLarge,
    /// Reading local stdin failed.
    Io(std::io::Error),
}

impl StdinError {
    /// Report as a typed response and fail.
    fn into_exit(self) -> ExitCode {
        match self {
            StdinError::TooLarge => fail_hint(
                ErrorCode::BadRequest,
                format!("stdin exceeds the {INLINE_STDIN_CAP}-byte inline cap"),
                "use `rx start --pipe HOST -- CMD`, which streams stdin unbounded",
            ),
            StdinError::Io(e) => fail(ErrorCode::Internal, format!("reading stdin: {e}")),
        }
    }
}

/// Read piped local stdin up to `INLINE_STDIN_CAP`. Returns None if stdin is a
/// terminal (no piped input). Errors if input exceeds the cap.
fn read_inline_stdin() -> Result<Option<Vec<u8>>, StdinError> {
    if std::io::stdin().is_terminal() {
        return Ok(None);
    }
    let mut buf = Vec::new();
    let mut handle = std::io::stdin().lock().take((INLINE_STDIN_CAP + 1) as u64);
    handle.read_to_end(&mut buf).map_err(StdinError::Io)?;
    if buf.len() > INLINE_STDIN_CAP {
        return Err(StdinError::TooLarge);
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum PipeStdinReport {
    Quiet,
    Written,
}

struct PipeCopy {
    bytes: u64,
    broken_pipe: bool,
}

fn copy_local_stdin_to_remote(remote_stdin: &mut impl std::io::Write) -> std::io::Result<PipeCopy> {
    use std::io::ErrorKind;

    let mut local_stdin = std::io::stdin().lock();
    let mut buf = [0u8; 64 * 1024];
    let mut copied = 0u64;

    loop {
        let n = match local_stdin.read(&mut buf) {
            Ok(0) => {
                return Ok(PipeCopy {
                    bytes: copied,
                    broken_pipe: false,
                });
            }
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };

        let mut offset = 0;
        while offset < n {
            match remote_stdin.write(&buf[offset..n]) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        ErrorKind::WriteZero,
                        "failed to write piped stdin to ssh",
                    ));
                }
                Ok(written) => {
                    copied += written as u64;
                    offset += written;
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == ErrorKind::BrokenPipe => {
                    return Ok(PipeCopy {
                        bytes: copied,
                        broken_pipe: true,
                    });
                }
                Err(e) => return Err(e),
            }
        }
    }
}

fn print_pipe_error(response: Response) -> ExitCode {
    print_json_response(&response);
    exit_for(&response)
}

/// Pipe local stdin to a remote process via SSH pipe-stdin.
fn pipe_local_stdin_to_remote(
    host: &str,
    id: &str,
    no_close: bool,
    report: PipeStdinReport,
) -> ExitCode {
    let args = RemoteArgs::pipe_stdin(id, no_close);
    let mut child = match ssh_spawn_piped_stdin(host, &args.as_str_slice()) {
        Ok(c) => c,
        Err(e) => {
            if report == PipeStdinReport::Written {
                return fail(
                    ErrorCode::Internal,
                    format!("failed to spawn pipe-stdin for {host}: {e}"),
                );
            }
            return ExitCode::FAILURE;
        }
    };

    let mut remote_stdin = child.stdin.take().unwrap();
    let copied = match copy_local_stdin_to_remote(&mut remote_stdin) {
        Ok(copied) => copied,
        Err(e) => {
            drop(remote_stdin);
            let _ = child.wait();
            if report == PipeStdinReport::Written {
                return print_pipe_error(Response::error_code(
                    rem_exec::protocol::ErrorCode::Internal,
                    format!("feeding stdin to {host}: {e}"),
                ));
            }
            return ExitCode::FAILURE;
        }
    };
    drop(remote_stdin);

    // OpenSSH reserves 255 for its own failures (unreachable, auth, mux), while
    // rxd only ever exits 0 or 1. That split is what lets us report a genuine
    // transport failure without also failing the ordinary case where pipe-stdin
    // exits 1 on EPIPE against a short-lived remote command.
    match child.wait() {
        Ok(s) if s.code() == Some(255) => {
            if report == PipeStdinReport::Written {
                return print_pipe_error(Response::error_code(
                    rem_exec::protocol::ErrorCode::SshUnreachable,
                    format!("ssh to {host} failed while feeding stdin to process {id}"),
                ));
            }
            ExitCode::FAILURE
        }
        Ok(s) if s.success() && !copied.broken_pipe => {
            if report == PipeStdinReport::Written {
                print_json_response(&Response::Written {
                    bytes: copied.bytes as usize,
                });
            }
            ExitCode::SUCCESS
        }
        Ok(s) => {
            if report == PipeStdinReport::Written {
                return print_pipe_error(Response::error_code(
                    rem_exec::protocol::ErrorCode::ProcessExited,
                    format!(
                        "process {id} on {host} did not accept piped stdin (rxd pipe-stdin exited with {s})"
                    ),
                ));
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            if report == PipeStdinReport::Written {
                return print_pipe_error(Response::error_code(
                    rem_exec::protocol::ErrorCode::Internal,
                    format!("waiting for ssh to {host}: {e}"),
                ));
            }
            ExitCode::FAILURE
        }
    }
}

fn preflight_piped_write(host: &str, id: &str) -> Option<ExitCode> {
    let response =
        match serve_request_auto_deploy(host, &Request::Status { id: id.to_string() }, &[]) {
            Ok(response) => response,
            Err(e) => {
                let response = transport_error_json(host, &e);
                print_json_response(&response);
                return Some(exit_for(&response));
            }
        };

    match response {
        Response::Status { state, .. } if state == "running" => None,
        Response::Status { state, .. } => {
            let response = Response::error_code(
                rem_exec::protocol::ErrorCode::ProcessExited,
                format!("process {id} on {host} is {state}; cannot write stdin"),
            );
            print_json_response(&response);
            Some(exit_for(&response))
        }
        response @ Response::Error { .. } => {
            print_json_response(&response);
            Some(exit_for(&response))
        }
        other => {
            let response = Response::error_code(
                rem_exec::protocol::ErrorCode::Internal,
                format!("unexpected status response before piped write: {other:?}"),
            );
            print_json_response(&response);
            Some(exit_for(&response))
        }
    }
}

/// Bidirectional pipe mode: stdin→remote stdin, remote stdout→local stdout.
/// The handle goes to stderr — via [`emit_json`] like every other object, so
/// this path is not a second mechanism that can drift from the first — and
/// stdout carries only data.
fn run_pipe_mode(host: &str, id: &str, response_data: &serde_json::Value) -> ExitCode {
    print_json(response_data);

    let host_stdin = host.to_string();
    let id_stdin = id.to_string();

    // Thread 1: local stdin → remote stdin
    let stdin_thread = thread::spawn(move || {
        pipe_local_stdin_to_remote(&host_stdin, &id_stdin, false, PipeStdinReport::Quiet);
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
            keep,
        } => {
            // Arguments are validated before stdin is touched. Reading first
            // would block on an idle pipe for a call that was never going to
            // run, and would swallow whatever a producer had already written
            // for a command that is about to be rejected.
            let env = match parse_env(env) {
                Ok(m) => m,
                Err(e) => return fail(ErrorCode::BadRequest, e),
            };
            let body = match read_inline_stdin() {
                Ok(b) => b.unwrap_or_default(),
                Err(e) => return e.into_exit(),
            };
            let request = Request::Run {
                command: cmd.clone(),
                cwd: cwd.clone(),
                env,
                timeout_ms: timeout.map(|s| s.saturating_mul(1000)),
                keep_stdin_open: *keep_stdin_open,
                ephemeral: !*keep,
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
                Err(e) => return fail(ErrorCode::BadRequest, e),
            };
            let needs_pipe = *pipe || !std::io::stdin().is_terminal();
            let request = Request::Start {
                command: cmd.clone(),
                cwd: cwd.clone(),
                env,
            };
            // Classified like every other transport failure, so an unreachable
            // host or a missing rxd reads as `ssh_unreachable`/`not_deployed`
            // here too — `start` used to be the one command where that answer
            // arrived as an untyped line.
            let response = match serve_request_auto_deploy(host, &request, &[]) {
                Ok(r) => r,
                Err(e) => {
                    let response = transport_error_json(host, &e);
                    print_json_response(&response);
                    return exit_for(&response);
                }
            };
            match &response {
                Response::Started { id } if needs_pipe => {
                    if *pipe {
                        let data = serde_json::to_value(&response).unwrap_or_default();
                        run_pipe_mode(host, id, &data)
                    } else {
                        print_json_response(&response);
                        pipe_local_stdin_to_remote(
                            host,
                            id,
                            *no_close_stdin,
                            PipeStdinReport::Quiet,
                        )
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
                if let Some(exit) = preflight_piped_write(host, id) {
                    return exit;
                }
                return pipe_local_stdin_to_remote(host, id, true, PipeStdinReport::Written);
            }
            let input = match input {
                Some(s) => s,
                None => {
                    return fail_hint(
                        ErrorCode::BadRequest,
                        "no input provided and stdin is not piped",
                        "pass the text as an argument (`rx write HOST ID TEXT`) or pipe it in",
                    );
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
        | Command::Ping { .. }
        | Command::Get { .. }
        | Command::Put { .. }
        | Command::Cache { .. }
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
            let response = transport_error_json(host, &e);
            print_json_response(&response);
            exit_for(&response)
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
            let response = transport_error_json(host, &e);
            print_json_response(&response);
            exit_for(&response)
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
    rem_exec::protocol::parse_octal_mode(s)
        .map_err(|_| format!("invalid --mode '{s}' (expected octal like 0644)"))
}

/// Deploy rxd to each host in turn, reporting one result per host. Explicit, so
/// it may fetch the matching binary unless `--offline`; the deploy policy only
/// governs deploys that happen as a side effect of other commands.
fn do_deploy(
    hosts: &[String],
    binary: Option<std::path::PathBuf>,
    offline: bool,
    allow_downgrade: bool,
) -> ExitCode {
    let opts = rem_exec::deploy::DeployOpts {
        binary,
        allow_fetch: !offline,
        allow_downgrade,
    };

    let mut results = Vec::with_capacity(hosts.len());
    let mut failed = false;
    for host in hosts {
        match rem_exec::deploy::deploy_to_host_with(host, &opts) {
            // "current" means the host already ran this exact rxd and nothing
            // was uploaded; "deployed" means it did work. `changed` carries the
            // same distinction in the form the rest of the contract uses.
            Ok(result) => results.push(serde_json::json!({
                "host": result.host,
                "arch": result.arch,
                "version": result.version,
                "status": if result.changed { "deployed" } else { "current" },
                "changed": result.changed,
            })),
            Err(e) => {
                failed = true;
                let response = rem_exec::deploy::deploy_error_response(host, &e);

                // Deploying one host is the first-contact case, and it answers
                // with the same typed error every other command produces —
                // rather than a `type:"deployed"` object with `status:"failed"`
                // inside it, which is a failure an agent has to be taught to
                // recognize.
                if hosts.len() == 1 {
                    print_json_response(&response);
                    return exit_for(&response);
                }

                // A batch keeps the per-host aggregate (some hosts succeeded),
                // but each failure carries the same code/retryable/hint as the
                // single-host error instead of a bare message string.
                eprintln!("error: {host}: {e}");
                let mut entry = serde_json::json!({"host": host, "status": "failed"});
                if let (Some(obj), Ok(serde_json::Value::Object(fields))) =
                    (entry.as_object_mut(), serde_json::to_value(&response))
                {
                    for key in ["code", "message", "retryable", "hint"] {
                        if let Some(value) = fields.get(key) {
                            obj.insert(key.to_string(), value.clone());
                        }
                    }
                }
                results.push(entry);
            }
        }
    }

    // One host stays a flat object; several become a list, so the common case
    // reads exactly as it did before. `type` is inserted first so it leads the
    // object once key order is preserved.
    if let [only] = results.as_slice() {
        let mut value = serde_json::json!({"type": "deployed"});
        if let (Some(obj), Some(fields)) = (value.as_object_mut(), only.as_object()) {
            for (key, field) in fields {
                obj.insert(key.clone(), field.clone());
            }
        }
        print_json(&value);
    } else {
        print_json(&serde_json::json!({"type": "deployed", "hosts": results}));
    }

    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Probe a host, annotating the reply with whether its rxd matches this rx.
/// `ping` is the health probe, so version skew is reported here rather than as
/// a warning on every command — a 0.2.x rxd is still correct for everything
/// except the newest requests, and crying wolf on each call would train an
/// agent to ignore it.
fn do_ping(host: &str) -> ExitCode {
    match serve_request_auto_deploy(host, &Request::Ping, &[]) {
        Ok(resp) => {
            let mut value = serde_json::to_value(&resp).unwrap_or(serde_json::Value::Null);
            if let Response::Ping {
                version, protocol, ..
            } = &resp
                && let Some(obj) = value.as_object_mut()
            {
                let local = env!("CARGO_PKG_VERSION");
                let matched = *protocol == rem_exec::protocol::PROTOCOL_VERSION && version == local;
                obj.insert("local_version".into(), serde_json::json!(local));
                obj.insert("up_to_date".into(), serde_json::json!(matched));
                if !matched {
                    // Point at the end that is actually behind: telling someone
                    // to deploy onto a newer host would only earn a refusal.
                    let hint = if rem_exec::deploy::is_newer_than_own(version) {
                        format!(
                            "{host} runs rxd {version}, ahead of rx {local} — upgrade rx rather \
                             than deploying onto it"
                        )
                    } else {
                        format!("run `rx deploy {host}` to match rx {local}")
                    };
                    obj.insert("hint".into(), serde_json::json!(hint));
                }
            }
            print_json(&value);
            exit_for(&resp)
        }
        Err(e) => {
            let response = transport_error_json(host, &e);
            print_json_response(&response);
            exit_for(&response)
        }
    }
}

/// Write a local file or stdin to `HOST:PATH`. Runs direct (not through the
/// daemon).
fn do_put(
    local: &str,
    remote: &str,
    mode: Option<&str>,
    owner: Option<String>,
    group: Option<String>,
    allow_empty: bool,
) -> ExitCode {
    let (host, path) = match remote.split_once(':') {
        Some((h, p)) if !h.is_empty() && !p.is_empty() => (h, p),
        _ => {
            return fail_hint(
                ErrorCode::BadRequest,
                format!("destination {remote:?} is not HOST:PATH"),
                "write it as one argument, e.g. `rx put ./app.conf host:/etc/app.conf`",
            );
        }
    };
    let mode = match mode {
        Some(s) => match parse_mode(s) {
            Ok(m) => Some(m),
            Err(e) => return fail(ErrorCode::BadRequest, e),
        },
        None => None,
    };

    let result = if local == "-" {
        put_stdin(host, path, mode, owner, group, allow_empty)
    } else {
        put_file(local, host, path, mode, owner, group)
    };

    match result {
        Ok(resp) => {
            print_json_response(&resp);
            exit_for(&resp)
        }
        Err(e) => {
            let response = transport_error_json(host, &e);
            print_json_response(&response);
            exit_for(&response)
        }
    }
}

/// Stream a local file, declaring its size up front. Retryable: each attempt
/// re-opens and re-stats the file, so an auto-deploy retry starts clean.
fn put_file(
    local: &str,
    host: &str,
    path: &str,
    mode: Option<u32>,
    owner: Option<String>,
    group: Option<String>,
) -> rem_exec::error::Result<Response> {
    let send = || -> rem_exec::error::Result<Response> {
        // Local open/stat failures are caller mistakes on this machine. Return a
        // typed response so they never reach `transport_error_json` (which would
        // probe the host and can mislabel a missing local path as
        // `not_deployed`). Same class as local `get` write failures.
        let mut f = match std::fs::File::open(local) {
            Ok(f) => f,
            Err(e) => return Ok(local_put_source_error_json(local, &e)),
        };
        let size = match f.metadata() {
            Ok(m) => m.len(),
            Err(e) => return Ok(local_put_source_error_json(local, &e)),
        };
        let request = Request::Put {
            path: path.to_string(),
            size: Some(size),
            mode,
            owner: owner.clone(),
            group: group.clone(),
        };
        rem_exec::ssh::serve_request_stream(host, &request, &mut f)
    };

    match send() {
        Err(e)
            if rem_exec::deploy::policy().implicit_deploy()
                && rem_exec::deploy::should_auto_deploy(&e) =>
        {
            let opts = rem_exec::deploy::DeployOpts::for_policy(rem_exec::deploy::policy());
            match rem_exec::deploy::deploy_to_host_with(host, &opts) {
                Ok(_) => send(),
                Err(de) => Err(rem_exec::error::RemExecError::Ssh(format!(
                    "auto-deploy to {host} failed: {de} (original: {e})"
                ))),
            }
        }
        other => other,
    }
}

/// Stream stdin, framed so the receiver can tell a finished stream from a
/// severed one.
///
/// There is no retry here: stdin cannot be rewound, so anything that would
/// normally be fixed and retried has to be settled *before* the first byte is
/// read. Under a policy that permits it, that means probing the host and
/// deploying first; otherwise the transfer runs once and reports why it failed.
fn put_stdin(
    host: &str,
    path: &str,
    mode: Option<u32>,
    owner: Option<String>,
    group: Option<String>,
    allow_empty: bool,
) -> rem_exec::error::Result<Response> {
    if std::io::stdin().is_terminal() {
        // Caller mistake, not a transport failure: answer here as `bad_request`
        // so we never open SSH / mislabel this as `not_deployed`.
        return Ok(Response::error_code(
            rem_exec::protocol::ErrorCode::BadRequest,
            "`rx put -` reads the file from stdin, but stdin is a terminal — pipe something \
             into it, or pass a path instead of `-` (./- for a file named `-`)",
        )
        .with_hint(
            "pipe a producer into `rx put -`, e.g. `rxv get host/secret | rx put - host:/path`",
        ));
    }

    if rem_exec::deploy::policy().implicit_deploy() {
        use rem_exec::deploy::DeployStatus;
        // A protocol match is not enough here: a 0.2.x rxd speaks this protocol
        // but has no streamed put, and finding that out costs the stdin we
        // cannot rewind. So this one command also requires the host not be
        // *behind* us. Only provably-older triggers a deploy — a host that is
        // ahead already has the feature, and pushing our build over it would
        // downgrade someone else's rx.
        let needs_deploy = match rem_exec::deploy::remote_deploy_status(host) {
            DeployStatus::Current { version } => rem_exec::deploy::is_older_than_own(&version),
            DeployStatus::Unknown => false,
            _ => true,
        };
        if needs_deploy {
            let opts = rem_exec::deploy::DeployOpts::for_policy(rem_exec::deploy::policy());
            rem_exec::deploy::deploy_to_host_with(host, &opts).map_err(|de| {
                rem_exec::error::RemExecError::Ssh(format!("auto-deploy to {host} failed: {de}"))
            })?;
        }
    }

    let request = Request::PutStream {
        path: path.to_string(),
        mode,
        owner,
        group,
        allow_empty,
    };
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();
    let resp = rem_exec::ssh::serve_request_framed(host, &request, &mut handle)?;
    Ok(explain_old_rxd(host, resp))
}

/// An rxd too old to know `put_stream` rejects it as a malformed request. That
/// is a deploy problem wearing the wrong error code — relabel it so an agent
/// branches on `not_deployed` here exactly as it does everywhere else.
fn explain_old_rxd(host: &str, resp: Response) -> Response {
    use rem_exec::protocol::ErrorCode;
    if let Response::Error {
        code: Some(ErrorCode::BadRequest),
        message,
        ..
    } = &resp
        && message.contains("put_stream")
    {
        return Response::error_code(
            ErrorCode::NotDeployed,
            format!(
                "rxd on {host} is too old for `rx put -`: it does not understand streamed \
                 stdin transfers"
            ),
        )
        .with_hint(format!(
            "run `rx deploy {host}` to install rxd {}, then re-run the pipeline",
            env!("CARGO_PKG_VERSION")
        ));
    }
    resp
}

/// Outcome of streaming a download body into a local file.
#[derive(Debug)]
enum ReceiveError {
    /// Fewer bytes arrived than declared — the partial file was discarded.
    Incomplete { expected: u64, got: u64 },
    /// Local I/O failure (temp create, write, fsync, rename).
    Io(std::io::Error),
}

/// A fully-received payload sitting in a private temp file, not yet visible at
/// the destination.
///
/// Receiving and installing are two steps so that a caller can interpose a
/// check between "all the bytes arrived" and "the file is installed" — `get`
/// uses the gap to verify the remote exit status. A download that fails either
/// half leaves no file behind, the same guarantee `cp` makes on the remote side.
struct StagedFile {
    tmp: std::path::PathBuf,
    bytes: u64,
}

impl StagedFile {
    /// Discard without installing. Used when a post-receive check fails.
    fn discard(self) {
        let _ = std::fs::remove_file(&self.tmp);
    }
}

fn get_temp_path(dir: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    dir.join(format!(".rxd-get-{suffix}.tmp"))
}

fn random_get_temp_suffix() -> std::io::Result<String> {
    let a = rem_exec::process::generate_id()
        .map_err(|e| std::io::Error::other(format!("generate temp name: {e}")))?;
    let b = rem_exec::process::generate_id()
        .map_err(|e| std::io::Error::other(format!("generate temp name: {e}")))?;
    Ok(format!("{a}{b}"))
}

fn create_get_temp_with_suffix(
    dir: &std::path::Path,
    suffix: &str,
) -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    use std::os::unix::fs::OpenOptionsExt;

    let tmp = get_temp_path(dir, suffix);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    Ok((tmp, file))
}

fn is_get_temp_name_collision(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::AlreadyExists || e.raw_os_error() == Some(libc::ELOOP)
}

fn create_get_temp(dir: &std::path::Path) -> std::io::Result<(std::path::PathBuf, std::fs::File)> {
    const ATTEMPTS: usize = 128;

    for _ in 0..ATTEMPTS {
        let suffix = random_get_temp_suffix()?;
        match create_get_temp_with_suffix(dir, &suffix) {
            Ok(created) => return Ok(created),
            Err(e) if is_get_temp_name_collision(&e) => continue,
            Err(e) => return Err(e),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not create a unique local staging file",
    ))
}

/// Stream exactly `size` bytes into a private temp beside `dest` and fsync it.
/// Any shortfall removes the temp and reports `Incomplete`.
fn receive_to_temp<R: std::io::Read>(
    src: &mut R,
    dest: &std::path::Path,
    size: u64,
) -> Result<StagedFile, ReceiveError> {
    let dir = match dest.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    let (tmp, mut f) = create_get_temp(&dir).map_err(ReceiveError::Io)?;

    let failed = |f: std::fs::File, tmp: &std::path::Path, e: ReceiveError| {
        drop(f);
        let _ = std::fs::remove_file(tmp);
        e
    };

    let copied = match std::io::copy(&mut (&mut *src).take(size), &mut f) {
        Ok(n) => n,
        Err(e) => return Err(failed(f, &tmp, ReceiveError::Io(e))),
    };
    if copied != size {
        return Err(failed(
            f,
            &tmp,
            ReceiveError::Incomplete {
                expected: size,
                got: copied,
            },
        ));
    }
    if let Err(e) = f.sync_all() {
        return Err(failed(f, &tmp, ReceiveError::Io(e)));
    }

    Ok(StagedFile { tmp, bytes: copied })
}

/// Apply `mode` and rename the staged file into place. A failure leaves nothing
/// behind, so the destination is either the old file or the complete new one.
fn commit_temp(staged: StagedFile, dest: &std::path::Path, mode: u32) -> Result<(), ReceiveError> {
    use std::os::unix::fs::PermissionsExt;

    let install = || -> std::io::Result<()> {
        std::fs::set_permissions(&staged.tmp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&staged.tmp, dest)
    };
    if let Err(e) = install() {
        let _ = std::fs::remove_file(&staged.tmp);
        return Err(ReceiveError::Io(e));
    }
    Ok(())
}

/// Turn a LOCAL filesystem failure during `get` into a typed response.
///
/// These deliberately do not travel as `RemExecError::Io` to
/// [`transport_error_json`]: the remote answered correctly and the failure is on
/// this machine. Routing them through the transport classifier dropped the
/// `code` entirely — the one shape `docs/CONTRACT.md` tells callers to branch on
/// — and paid a `remote_deploy_status` round trip to diagnose a local directory.
fn local_io_error_json(local: &str, e: &std::io::Error) -> Response {
    let resp = Response::error_code(
        rem_exec::protocol::io_error_code(e),
        format!("cannot write {local}: {e}"),
    );
    match e.kind() {
        std::io::ErrorKind::NotFound => resp.with_hint(
            "the local parent directory does not exist — create it, or pick another path",
        ),
        std::io::ErrorKind::PermissionDenied => {
            resp.with_hint("no write permission for that local path — pick another destination")
        }
        _ => resp,
    }
}

/// Local source failure for `put` (open/stat of the file being uploaded).
///
/// Same reason as [`local_io_error_json`]: never route through the transport
/// classifier. A missing local path is `not_found`, not `not_deployed`.
fn local_put_source_error_json(local: &str, e: &std::io::Error) -> Response {
    let resp = Response::error_code(
        rem_exec::protocol::io_error_code(e),
        format!("cannot read {local}: {e}"),
    );
    match e.kind() {
        std::io::ErrorKind::NotFound => resp.with_hint(
            "the local source path does not exist — check the path, or use `-` to read stdin",
        ),
        std::io::ErrorKind::PermissionDenied => {
            resp.with_hint("no read permission for that local path")
        }
        _ => resp,
    }
}

/// Download HOST:PATH to a local file, streaming and atomic, with the same
/// completeness guarantee as `cp`. Honors auto-deploy like other commands.
fn do_get(remote: &str, local: &str, mode: Option<&str>) -> ExitCode {
    use rem_exec::protocol::ErrorCode;
    use std::io::{BufRead, Read};

    let (host, path) = match remote.split_once(':') {
        Some((h, p)) if !h.is_empty() && !p.is_empty() => (h, p),
        _ => {
            return fail_hint(
                ErrorCode::BadRequest,
                format!("source {remote:?} is not HOST:PATH"),
                "write it as one argument, e.g. `rx get host:/var/log/app.log ./app.log`",
            );
        }
    };
    let mode_override = match mode {
        Some(s) => match parse_mode(s) {
            Ok(m) => Some(m),
            Err(e) => return fail(ErrorCode::BadRequest, e),
        },
        None => None,
    };
    let request = Request::Get {
        path: path.to_string(),
    };

    let fetch = || -> rem_exec::error::Result<Response> {
        let mut child = rem_exec::ssh::serve_stream_download(host, &request)?;
        let stdout = child.stdout.take().expect("stdout was piped");
        let mut stderr = child.stderr.take().expect("stderr was piped");
        let mut reader = std::io::BufReader::new(stdout);

        let mut line = Vec::new();
        reader
            .read_until(b'\n', &mut line)
            .map_err(rem_exec::error::RemExecError::Io)?;
        if line.is_empty() {
            // No response — likely a missing/old rxd; surface ssh stderr so the
            // auto-deploy classifier can see "No such file".
            let mut err = String::new();
            let _ = stderr.read_to_string(&mut err);
            let _ = child.wait();
            return Err(rem_exec::error::RemExecError::Ssh(format!(
                "no response from remote: {}",
                err.trim()
            )));
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        let header: Response = serde_json::from_slice(&line).map_err(|e| {
            rem_exec::error::RemExecError::Protocol(format!("invalid header from remote: {e}"))
        })?;

        match header {
            Response::Error { .. } => {
                let _ = child.wait();
                Ok(header) // typed error (not_found, etc.) — print as-is
            }
            Response::GetStream {
                size,
                mode: src_mode,
            } => {
                let src_mode = rem_exec::protocol::parse_octal_mode(&src_mode)
                    .map_err(rem_exec::error::RemExecError::Protocol)?;
                let applied = mode_override.unwrap_or(src_mode);
                let dest = std::path::Path::new(local);
                let staged = match receive_to_temp(&mut reader, dest, size) {
                    Ok(s) => s,
                    Err(ReceiveError::Incomplete { expected, got }) => {
                        let _ = child.wait();
                        return Ok(Response::error_code(
                            ErrorCode::IncompleteTransfer,
                            format!(
                                "incomplete transfer from {path}: expected {expected} bytes, received {got}"
                            ),
                        ));
                    }
                    Err(ReceiveError::Io(e)) => {
                        let _ = child.wait();
                        return Ok(local_io_error_json(local, &e));
                    }
                };

                // All the declared bytes arrived. Two things still have to be
                // checked before this counts as a copy of the file.
                //
                // First: is there anything after them? A current rxd bounds the
                // body to the declared size, so the answer is no. An rxd from
                // before that fix copies to EOF and keeps writing as the file
                // grows — which both proves the file changed and, if we simply
                // stopped reading, would wedge it on a full pipe while we waited
                // for it to exit. So drain a bounded amount rather than either
                // trusting the count or reading forever, then close the pipe so
                // a still-writing remote gets EPIPE and terminates.
                const DRAIN_CAP: u64 = 64 * 1024;
                let extra = std::io::copy(&mut (&mut reader).take(DRAIN_CAP), &mut std::io::sink())
                    .unwrap_or(0);
                drop(reader);
                drop(stderr);

                // Second: rxd re-stats the file and exits non-zero when its
                // length moved. Both checks happen before the rename, so a file
                // that changed leaves the destination exactly as it was.
                let status = child.wait().map_err(rem_exec::error::RemExecError::Io)?;
                if extra > 0 || !status.success() {
                    staged.discard();
                    return Ok(Response::error_code(
                        ErrorCode::FileChanged,
                        format!(
                            "{path} changed while being read — nothing was written to {local}; \
                             retry, or snapshot the file remotely first"
                        ),
                    ));
                }

                let bytes = staged.bytes;
                match commit_temp(staged, dest, applied) {
                    Ok(()) => Ok(Response::Got {
                        path: local.to_string(),
                        bytes,
                        mode: Some(rem_exec::protocol::octal_mode(applied)),
                    }),
                    Err(ReceiveError::Incomplete { .. }) => unreachable!("commit cannot be short"),
                    Err(ReceiveError::Io(e)) => Ok(local_io_error_json(local, &e)),
                }
            }
            _ => Err(rem_exec::error::RemExecError::Protocol(
                "unexpected header type from remote".to_string(),
            )),
        }
    };

    let result = match fetch() {
        Err(e)
            if rem_exec::deploy::auto_deploy_enabled()
                && rem_exec::deploy::should_auto_deploy(&e) =>
        {
            // Honor RX_AUTO_DEPLOY=local the same way put/serve do: repair from
            // the cache without downloading. deploy_to_host() always allows fetch.
            let opts = rem_exec::deploy::DeployOpts::for_policy(rem_exec::deploy::policy());
            match rem_exec::deploy::deploy_to_host_with(host, &opts) {
                Ok(_) => fetch(),
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
            let response = transport_error_json(host, &e);
            print_json_response(&response);
            exit_for(&response)
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
                Err(e) => return fail(ErrorCode::BadRequest, e),
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
                    pipe_local_stdin_to_remote(host, id, *no_close_stdin, PipeStdinReport::Quiet)
                }
                Ok(DaemonResponse::Error { message }) => {
                    let response = daemon_error_json(message);
                    print_json_response(&response);
                    exit_for(&response)
                }
                Err(e) => fail(ErrorCode::Internal, format!("local daemon: {e}")),
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
            keep,
        } => {
            // Validate before reading stdin — see the direct-SSH path.
            let env = match parse_env(env) {
                Ok(m) => m,
                Err(e) => return fail(ErrorCode::BadRequest, e),
            };
            let stdin_b64 = match read_inline_stdin() {
                Ok(Some(b)) if !b.is_empty() => Some(rem_exec::base64_encode(&b)),
                Ok(_) => None,
                Err(e) => return e.into_exit(),
            };
            DaemonRequest::Run {
                host: host.clone(),
                command: cmd.clone(),
                cwd: cwd.clone(),
                env,
                timeout_ms: timeout.map(|s| s.saturating_mul(1000)),
                stdin_b64,
                keep_stdin_open: *keep_stdin_open,
                ephemeral: !*keep,
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
                Err(e) => return fail(ErrorCode::BadRequest, e),
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
                if let Some(exit) = preflight_piped_write(host, id) {
                    return exit;
                }
                return pipe_local_stdin_to_remote(host, id, true, PipeStdinReport::Written);
            }
            let input = match input {
                Some(s) => s,
                None => {
                    return fail_hint(
                        ErrorCode::BadRequest,
                        "no input provided and stdin is not piped",
                        "pass the text as an argument (`rx write HOST ID TEXT`) or pipe it in",
                    );
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
        | Command::Ping { .. }
        | Command::Get { .. }
        | Command::Put { .. }
        | Command::Cache { .. }
        | Command::Skill
        | Command::Daemon { .. } => unreachable!("handled before routing"),
    };

    let is_run = matches!(command, Command::Run { .. } | Command::Wait { .. });
    match daemon::send_request(&request) {
        Ok(DaemonResponse::Ok { data }) => {
            print_json(&data);
            if data.get("type").and_then(|v| v.as_str()) == Some("error") {
                error_exit_from_value(&data)
            } else if is_run {
                run_exit_from_value(&data)
            } else {
                ExitCode::SUCCESS
            }
        }
        Ok(DaemonResponse::Error { message }) => {
            let response = daemon_error_json(message);
            print_json_response(&response);
            exit_for(&response)
        }
        Err(e) => fail(ErrorCode::Internal, format!("local daemon: {e}")),
    }
}

/// Exit code for a non-run response: FAILURE only for an error response.
/// Every SSH destination this command will touch.
///
/// Exhaustive on purpose — no `_` arm — so a new host-bearing subcommand fails
/// to compile until it is listed here rather than silently skipping validation.
/// For the `HOST:PATH` forms an unsplittable argument yields nothing; the
/// command's own "must be HOST:PATH" error is the better message there.
/// The process ID a command carries, if it takes one.
///
/// Kept beside [`command_hosts`] and exhaustive for the same reason: a new
/// ID-taking command must not silently skip validation, so this matches every
/// variant rather than ending in a wildcard.
fn command_process_id(command: &Command) -> Option<&str> {
    match command {
        Command::Wait { id, .. }
        | Command::Status { id, .. }
        | Command::Stdout { id, .. }
        | Command::Stderr { id, .. }
        | Command::Write { id, .. }
        | Command::CloseStdin { id, .. }
        | Command::Kill { id, .. } => Some(id.as_str()),
        Command::Run { .. }
        | Command::Start { .. }
        | Command::List { .. }
        | Command::Clean { .. }
        | Command::Ping { .. }
        | Command::Deploy { .. }
        | Command::Put { .. }
        | Command::Get { .. }
        | Command::Daemon { .. }
        | Command::Cache { .. }
        | Command::Skill => None,
    }
}

fn command_hosts(command: &Command) -> Vec<&str> {
    fn host_of(spec: &str) -> Option<&str> {
        match spec.split_once(':') {
            Some((h, p)) if !h.is_empty() && !p.is_empty() => Some(h),
            _ => None,
        }
    }
    match command {
        Command::Run { host, .. }
        | Command::Start { host, .. }
        | Command::Wait { host, .. }
        | Command::Status { host, .. }
        | Command::Stdout { host, .. }
        | Command::Stderr { host, .. }
        | Command::Write { host, .. }
        | Command::CloseStdin { host, .. }
        | Command::Kill { host, .. }
        | Command::List { host, .. }
        | Command::Clean { host, .. }
        | Command::Ping { host, .. } => vec![host.as_str()],
        Command::Deploy { hosts, .. } => hosts.iter().map(String::as_str).collect(),
        Command::Put { remote, .. } | Command::Get { remote, .. } => {
            host_of(remote).into_iter().collect()
        }
        Command::Daemon { .. } | Command::Cache { .. } | Command::Skill => Vec::new(),
    }
}

/// Map a response to the process exit code, deriving a failure's status from its
/// own `code` so exit 2 means "this call is unusable as written" wherever the
/// rejection came from — rx's own checks, clap, or rxd on the far side.
fn exit_for(response: &Response) -> ExitCode {
    match response {
        Response::Error { code, .. } => error_exit(*code),
        _ => ExitCode::SUCCESS,
    }
}

/// Exit status for a typed error. An error that reached here without a code is
/// exit 1: `internal` is what an unclassified failure means, and that is a
/// failed operation, not a malformed call.
fn error_exit(code: Option<ErrorCode>) -> ExitCode {
    ExitCode::from(code.map_or(1, ErrorCode::exit_code))
}

/// [`error_exit`] for an error that arrived as an opaque value — what the local
/// daemon hands back. Routing through the daemon must not change the exit
/// status, the same reason `run` reads its remote status out of the value here
/// rather than defaulting to success.
fn error_exit_from_value(data: &serde_json::Value) -> ExitCode {
    let code = data
        .get("code")
        .and_then(|c| serde_json::from_value::<ErrorCode>(c.clone()).ok());
    error_exit(code)
}

/// Exit status for a command that never started, following the shell
/// convention for "command not found".
///
/// exec failure used to exit 0: the JSON said `exec_error:"command_not_found"`
/// while the process reported success, so `rx run HOST -- missing-tool &&
/// next-step` ran the next step. The JSON stays the source of truth, but a
/// convenience that is actively wrong is worse than no convenience.
const EXEC_FAILED_EXIT: u8 = 127;

/// Exit code for a `run`: propagate the remote command's exit status so
/// `rx run host false` behaves like a normal command runner. Agents still read
/// the structured result from the JSON.
fn run_exit(response: &Response) -> ExitCode {
    match response {
        Response::Completed {
            exit_code,
            signal,
            exec_error,
            ..
        } => {
            if exec_error.is_some() {
                ExitCode::from(EXEC_FAILED_EXIT)
            } else if let Some(code) = exit_code {
                ExitCode::from((*code).clamp(0, 255) as u8)
            } else if let Some(sig) = signal {
                ExitCode::from((128 + *sig).clamp(0, 255) as u8)
            } else {
                ExitCode::SUCCESS
            }
        }
        Response::Error { code, .. } => error_exit(*code),
        _ => ExitCode::SUCCESS,
    }
}

fn run_exit_from_value(data: &serde_json::Value) -> ExitCode {
    if data.get("type").and_then(|v| v.as_str()) == Some("completed") {
        // Checked before exit_code/signal, which are both null in this case.
        if data.get("exec_error").is_some_and(|v| !v.is_null()) {
            return ExitCode::from(EXEC_FAILED_EXIT);
        }
        if let Some(code) = data.get("exit_code").and_then(|v| v.as_i64()) {
            return ExitCode::from(code.clamp(0, 255) as u8);
        }
        if let Some(sig) = data.get("signal").and_then(|v| v.as_i64()) {
            return ExitCode::from((128 + sig).clamp(0, 255) as u8);
        }
    }
    ExitCode::SUCCESS
}

/// Turn a transport failure into a JSON error `Response`. Connectivity/auth
/// failures classify into typed codes (`ssh_unreachable` / `ssh_auth`); an
/// ambiguous failure triggers a `version` probe so a missing/outdated rxd
/// surfaces as an actionable `not_deployed` (with a `rx deploy` /
/// `RX_AUTO_DEPLOY` hint) instead of a cryptic error an agent would give up
/// on and fall back to raw ssh. Always a JSON object on stdout.
fn transport_error_json(host: &str, e: &rem_exec::error::RemExecError) -> Response {
    let message = e.to_string();
    if let Some(response) = classify_transport_message(&message) {
        return response;
    }
    let status = rem_exec::deploy::remote_deploy_status(host);
    if matches!(
        status,
        rem_exec::deploy::DeployStatus::Missing
            | rem_exec::deploy::DeployStatus::Incompatible { .. }
    ) {
        return rem_exec::deploy::not_deployed_response(host, &status);
    }
    // Nothing classified it, but every error still carries a code: `internal` is
    // the contract's "no better answer available", and an agent that switches on
    // `code` must never fall through to `undefined`.
    Response::error_code(rem_exec::protocol::ErrorCode::Internal, message)
}

/// Classify a daemon-relayed error message. The daemon already forwarded the
/// request and this path has no host to probe, so SSH-classify and auto-deploy
/// prefix only — no deploy probe. Auto-deploy failures must stay `not_deployed`
/// here too, or `RX_DAEMON=1` silently changes the codes an agent branches on.
fn daemon_error_json(message: String) -> Response {
    if let Some(response) = classify_transport_message(&message) {
        return response;
    }
    Response::error_code(rem_exec::protocol::ErrorCode::Internal, message)
}

/// Shared transport-message classification for the direct-SSH and daemon paths.
///
/// Returns `Some` when the message alone is enough (SSH phrases, auto-deploy
/// prefix). Returns `None` when the direct path should still probe deploy
/// status; the daemon path treats that as `internal` (no host to probe).
fn classify_transport_message(message: &str) -> Option<Response> {
    if let Some(code) = rem_exec::ssh::classify_ssh_failure(message) {
        let response = Response::error_code(code, message.to_string());
        // The hint belongs to the code, not to this call site: `deploy` reports
        // the same failures and must name the same fix.
        return Some(match rem_exec::ssh::transport_hint(code) {
            Some(hint) => response.with_hint(hint),
            None => response,
        });
    }
    // A failed auto-deploy already carries the precise reason (e.g. a missing
    // local cache → "run `rx cache fetch` first"); surface it verbatim instead of
    // re-probing into a generic not_deployed.
    if message.contains("auto-deploy to ") {
        return Some(Response::error_code(
            rem_exec::protocol::ErrorCode::NotDeployed,
            message.to_string(),
        ));
    }
    None
}

/// Which stream this command's JSON object goes to.
///
/// stdout carries the *product*. For almost every command the product is the
/// object itself; for `start --pipe` it is the remote process's bytes, so the
/// object — the handle on success, the typed error on failure — goes to stderr
/// instead and stdout stays byte-clean. rxv applies the same rule to `get`.
///
/// `start` WITHOUT `--pipe` is not an exception: it forwards local stdin but
/// never puts remote stdout on ours, so its object belongs on stdout.
fn object_goes_to_stdout(command: &Command) -> bool {
    !matches!(command, Command::Start { pipe: true, .. })
}

/// Print one JSON object to the stream [`object_goes_to_stdout`] assigns it.
fn emit_json(text: &str) {
    if OBJECT_TO_STDERR.load(Ordering::Relaxed) {
        write_line(&mut std::io::stderr(), text);
    } else {
        write_line(&mut std::io::stdout(), text);
    }
}

/// Write a line, treating a closed consumer as an ordinary end rather than a
/// crash.
///
/// The Rust runtime ignores SIGPIPE, so a write to a closed stdout returns EPIPE
/// and the `println!` family panics on it: `rx run HOST -- <big output> | head`
/// printed a panic banner and exited **101**, a status the contract does not
/// define — and only once the payload outgrew the pipe buffer, so small outputs
/// hid it.
///
/// Restoring the default SIGPIPE disposition would be the usual fix for a
/// filter, but it is wrong here: rx forks a daemon that must survive a client
/// hanging up mid-response, and its own pipe paths handle EPIPE deliberately and
/// still owe the caller an object afterwards. Both would have died silently.
/// Dropping the write keeps every one of those paths intact, and the exit status
/// still describes the operation rather than the consumer.
fn write_line(stream: &mut impl std::io::Write, text: &str) {
    write_out(stream, format_args!("{text}\n"));
}

/// [`write_line`] without the newline, for the guide.
fn write_all(stream: &mut impl std::io::Write, text: &str) {
    write_out(stream, format_args!("{text}"));
}

fn write_out(stream: &mut impl std::io::Write, args: std::fmt::Arguments<'_>) {
    use std::io::ErrorKind;
    match stream.write_fmt(args) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::BrokenPipe => {}
        Err(e) => eprintln!("rx: cannot write output: {e}"),
    }
}

fn print_json_response(response: &Response) {
    emit_json(&render_json(response));
}

/// Collapse a multi-line rendering into one line for a JSON string field.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Split clap's rendering into a one-sentence `message` and, when clap offers
/// one, the `hint` that names the fix.
///
/// clap renders up to four blocks: the error line, an optional `tip:`, a
/// `Usage:` block, and "For more information, try '--help'". Flattening all four
/// into `message` produced a 200-character run-on in a field `docs/CONTRACT.md`
/// describes as one short sentence, and duplicated a usage block into a place
/// nothing can act on. The first line is the diagnosis; clap's tip is a concrete
/// different command, which is exactly what `hint` is for; the remainder is help
/// text the hint already points at.
fn clap_parts(rendered: &str) -> (String, Option<String>) {
    let mut message = String::new();
    let mut tip = None;
    for line in rendered.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(rest) = line.strip_prefix("error: ") {
            if message.is_empty() {
                message = one_line(rest);
            }
        } else if let Some(rest) = line.strip_prefix("tip: ") {
            tip.get_or_insert_with(|| one_line(rest));
        } else if message.is_empty() && !line.starts_with("Usage:") {
            message = one_line(line);
        }
    }
    if message.is_empty() {
        message = one_line(rendered);
    }
    (message, tip)
}

/// Whether the parse-failure object should be compact.
///
/// It goes to stderr, so stderr is what "follows the destination" means here —
/// [`compact_requested`] asks about stdout, which is empty on this path. The
/// parsed flags are unavailable because parsing is what failed, so an explicit
/// choice is read straight off the raw argv.
fn compact_for_stderr() -> bool {
    let explicit = std::env::args().find(|a| a == "--compact" || a == "--pretty");
    match explicit.as_deref() {
        Some("--compact") => return true,
        Some("--pretty") => return false,
        _ => {}
    }
    match json_env().as_deref() {
        Some("compact") => true,
        Some("pretty") => false,
        _ => !std::io::stderr().is_terminal(),
    }
}

/// Answer a clap rejection, splitting discovery from a mis-made call.
///
/// `--help`, `-h`, `help`, `--version` and a bare `rx` are discovery: they print
/// for a reader and emit no object, because a person's first keystroke should
/// not be answered with a JSON blob. Everything else — an unknown flag, a
/// missing argument, a bad enum value — is an operation the caller got wrong,
/// and gets the same typed object every other failure produces. 0.4.0 typed the
/// argument errors rx checks itself but left the parser's own rejections as
/// prose; they are the same class of mistake and the parser's are hit more
/// often.
///
/// The object goes to **stderr**: exit 2 promises an empty stdout, and at this
/// point the subcommand is not yet known — if it were `rxv get`, anything on
/// stdout would land in whatever the pipe feeds.
fn parse_failure(err: clap::Error) -> ExitCode {
    use clap::error::ErrorKind;

    match err.kind() {
        // clap routes help and version to stdout itself.
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
            let _ = err.print();
            ExitCode::SUCCESS
        }
        // A bare invocation is discovery too — it prints help, not an object —
        // but it named no operation, so it still fails.
        ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            let _ = err.print();
            ExitCode::from(2)
        }
        // The object is the *whole* of stderr here, never prose alongside it:
        // the contract promises a typed object on exit 2, and a caller doing
        // `JSON.parse(stderr)` must not trip over a usage block. clap's
        // diagnosis becomes `message` and its suggestion becomes `hint`, so
        // nothing an agent can act on is lost and neither field carries a page
        // of help text.
        _ => {
            COMPACT_JSON.store(compact_for_stderr(), Ordering::Relaxed);
            let rendered = strip_ansi(&err.render().to_string());
            let (message, tip) = clap_parts(&rendered);
            let hint = tip.map_or_else(
                || "run `rx --help`, or `rx skill` for the full guide".to_string(),
                |t| format!("{t}; `rx --help` for the full usage"),
            );
            let response = Response::error_code(ErrorCode::BadRequest, message).with_hint(hint);
            eprintln!("{}", render_json(&response));
            ExitCode::from(2)
        }
    }
}

/// Drop ANSI SGR sequences so clap's styled rendering does not end up as escape
/// codes inside a JSON string when stderr happens to be a terminal.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // ESC [ ... <final byte in @..~>
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

/// Report a client-side failure as a typed error object, exiting per its code.
///
/// The single place an argument rx cannot use becomes a response. The contract
/// promises exactly one JSON object per invocation, so a bad `--mode` has to
/// arrive in the same shape as an unreachable host: an agent that parses stdout
/// and branches on `code` never has to fall back to reading prose off stderr,
/// which is what it does right before it gives up on the tool.
///
/// The object still goes to the command's normal object stream — stdout for
/// almost everything, stderr under `start --pipe`, where stdout is the process's
/// bytes. Only the parser's own rejections are forced to stderr, and only
/// because the subcommand is not known yet at that point.
fn fail(code: ErrorCode, message: impl Into<String>) -> ExitCode {
    let response = Response::error_code(code, message);
    print_json_response(&response);
    exit_for(&response)
}

/// [`fail`] with a `hint` naming the fix. Use it when the fix is a *different*
/// command or flag; when the message already states the expected form, the hint
/// would only repeat it.
fn fail_hint(code: ErrorCode, message: impl Into<String>, hint: impl Into<String>) -> ExitCode {
    let response = Response::error_code(code, message).with_hint(hint);
    print_json_response(&response);
    exit_for(&response)
}

fn print_json(value: &serde_json::Value) {
    emit_json(&render_json(value));
}

/// Serialize a value as JSON, honoring the process-wide compact/pretty choice.
fn render_json<T: serde::Serialize>(value: &T) -> String {
    if COMPACT_JSON.load(Ordering::Relaxed) {
        serde_json::to_string(value)
    } else {
        serde_json::to_string_pretty(value)
    }
    .unwrap_or_default()
}

/// Print a daemon control result — or its failure — as one JSON object.
fn report(result: rem_exec::error::Result<serde_json::Value>) -> ExitCode {
    match result {
        Ok(value) => {
            print_json(&value);
            ExitCode::SUCCESS
        }
        Err(e) => fail(ErrorCode::Internal, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;

    /// A consumer that hung up, the way `| head` does.
    struct ClosedPipe;

    impl std::io::Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
    }

    /// A closed consumer ends the write, not the process.
    ///
    /// SIGPIPE is ignored by the Rust runtime, so this arrives as EPIPE and the
    /// `println!` family panics on it — which exited 101 with a panic banner,
    /// outside the 0/1/2 the contract defines. Restoring the default signal
    /// disposition was the other candidate and is wrong here: rx forks a daemon
    /// that must outlive a client hanging up, and its pipe paths handle EPIPE
    /// deliberately and still owe the caller an object.
    #[test]
    fn a_closed_consumer_does_not_panic_the_writer() {
        write_line(&mut ClosedPipe, "{\"type\":\"ping\"}");
        write_all(&mut ClosedPipe, "the guide, unread");
    }

    fn completed(
        exit_code: Option<i32>,
        signal: Option<i32>,
        exec_error: Option<&str>,
    ) -> Response {
        Response::Completed {
            id: "abcdef01".into(),
            exit_code,
            signal,
            exec_error: exec_error.map(str::to_string),
            duration_ms: 1,
            stdout: String::new(),
            stdout_encoding: rem_exec::protocol::Encoding::Utf8,
            stderr: String::new(),
            stderr_encoding: rem_exec::protocol::Encoding::Utf8,
            stdout_size: 0,
            stderr_size: 0,
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    /// A command that never started must not look like success to a shell.
    /// `rx run HOST -- missing-tool && next-step` used to run `next-step`.
    #[test]
    fn exec_failure_exits_127_not_zero() {
        let response = completed(None, None, Some("command_not_found"));
        assert_eq!(
            format!("{:?}", run_exit(&response)),
            format!("{:?}", ExitCode::from(127u8))
        );
    }

    /// Every ID-taking command must be covered. The match in
    /// [`command_process_id`] is exhaustive, so a new one is a compile error
    /// rather than a silently unvalidated path — this pins the current set.
    #[test]
    fn every_id_taking_command_is_validated_locally() {
        let with_id = |id: &str| Command::Status {
            host: "h".into(),
            id: id.into(),
        };
        assert_eq!(command_process_id(&with_id("deadbeef")), Some("deadbeef"));
        assert!(rem_exec::process::is_valid_process_id("deadbeef"));
        for bad in [
            "NOTANID",
            "deadbee",
            "deadbeef0",
            "",
            "dead beef",
            "ZZZZZZZZ",
        ] {
            assert_eq!(command_process_id(&with_id(bad)), Some(bad));
            assert!(
                !rem_exec::process::is_valid_process_id(bad),
                "{bad:?} must be rejected"
            );
        }
        // Commands that take no ID must not be caught by the guard.
        assert_eq!(command_process_id(&Command::Skill), None);
        assert_eq!(
            command_process_id(&Command::List { host: "h".into() }),
            None
        );
    }

    /// A local write failure during `get` is not a transport problem. It used to
    /// travel as `RemExecError::Io` into `transport_error_json`, which fell
    /// through to an error with NO `code` — the single field both skills tell
    /// callers to branch on — after paying a `remote_deploy_status` probe to
    /// diagnose a directory on this machine.
    #[test]
    fn a_local_get_failure_is_typed_and_names_the_local_path() {
        use std::io::{Error, ErrorKind};

        let missing = local_io_error_json("/no/such/dir/f", &Error::from(ErrorKind::NotFound));
        let value = serde_json::to_value(&missing).unwrap();
        assert_eq!(value["code"], "not_found", "{value}");
        assert_eq!(value["retryable"], false, "{value}");
        assert!(
            value["message"]
                .as_str()
                .unwrap()
                .contains("/no/such/dir/f")
        );
        assert!(value["hint"].is_string(), "{value}");

        let denied = local_io_error_json("/etc/f", &Error::from(ErrorKind::PermissionDenied));
        let value = serde_json::to_value(&denied).unwrap();
        assert_eq!(value["code"], "bad_request", "{value}");
        assert_eq!(value["retryable"], false, "{value}");
    }

    /// A local source failure during `put` is the same class: never a transport
    /// code, never a host probe.
    #[test]
    fn a_local_put_source_failure_is_typed_and_names_the_local_path() {
        use std::io::{Error, ErrorKind};

        let missing =
            local_put_source_error_json("/no/such/file", &Error::from(ErrorKind::NotFound));
        let value = serde_json::to_value(&missing).unwrap();
        assert_eq!(value["code"], "not_found", "{value}");
        assert_eq!(value["retryable"], false, "{value}");
        assert!(value["message"].as_str().unwrap().contains("/no/such/file"));
        assert!(value["hint"].is_string(), "{value}");
    }

    /// The contract advertises `code` unconditionally. `Response::error()` — the
    /// only constructor that could omit it — is gone, so this pins the one path
    /// that used it: a transport failure nothing else could classify.
    #[test]
    fn an_unclassifiable_transport_failure_still_carries_a_code() {
        let response = daemon_error_json("something the classifier has never seen".into());
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["type"], "error", "{value}");
        assert_eq!(value["code"], "internal", "{value}");
        assert!(value.get("retryable").is_some(), "{value}");
    }

    /// A failed auto-deploy message must stay `not_deployed` on the daemon path
    /// too: the codes an agent branches on must not depend on RX_DAEMON.
    #[test]
    fn a_daemon_auto_deploy_failure_is_not_deployed() {
        let response = daemon_error_json(
            "auto-deploy to host1 failed: run `rx cache fetch` first (original: …)".into(),
        );
        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["code"], "not_deployed", "{value}");
    }

    /// A real exit 127 and a failed exec share an exit status but stay
    /// distinguishable in the JSON, which is the source of truth.
    #[test]
    fn a_genuine_exit_127_is_still_propagated() {
        let response = completed(Some(127), None, None);
        assert_eq!(
            format!("{:?}", run_exit(&response)),
            format!("{:?}", ExitCode::from(127u8))
        );
    }

    #[test]
    fn ordinary_statuses_are_unaffected_by_the_exec_failure_path() {
        for (code, signal, want) in [
            (Some(0), None, 0u8),
            (Some(1), None, 1),
            (None, Some(9), 137),
        ] {
            assert_eq!(
                format!("{:?}", run_exit(&completed(code, signal, None))),
                format!("{:?}", ExitCode::from(want)),
                "exit_code={code:?} signal={signal:?}"
            );
        }
    }

    /// The daemon path reads the same shape out of a JSON value and must agree
    /// with `run_exit`, or the exit status would depend on RX_DAEMON.
    #[test]
    fn the_value_path_agrees_about_exec_failure() {
        let data = serde_json::json!({
            "type": "completed", "exit_code": null, "signal": null,
            "exec_error": "command_not_found",
        });
        assert_eq!(
            format!("{:?}", run_exit_from_value(&data)),
            format!("{:?}", ExitCode::from(127u8))
        );
    }

    #[test]
    fn daemon_opt_in_requires_an_explicit_truthy_value() {
        assert!(daemon_opt_in(Some("1")));
        assert!(daemon_opt_in(Some("true")));
        assert!(daemon_opt_in(Some("YES")));
        assert!(daemon_opt_in(Some(" on ")));
        assert!(!daemon_opt_in(Some("0")));
        assert!(!daemon_opt_in(Some("")));
        assert!(!daemon_opt_in(None));
    }

    // An explicit flag decides on its own; only an unforced call may consult
    // the environment or the terminal, which is what keeps a pipeline's output
    // shape independent of where it happens to run.
    #[test]
    fn explicit_json_flags_win_over_everything_else() {
        assert!(compact_requested(true, false));
        assert!(!compact_requested(false, true));
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rx-receive-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn staged_temp_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".rxd-get-") && name.ends_with(".tmp"))
            })
            .collect()
    }

    /// receive + install, the way `get` composes them on the happy path.
    fn receive_file<R: std::io::Read>(
        src: &mut R,
        dest: &std::path::Path,
        size: u64,
        mode: u32,
    ) -> Result<u64, ReceiveError> {
        let staged = receive_to_temp(src, dest, size)?;
        let bytes = staged.bytes;
        commit_temp(staged, dest, mode)?;
        Ok(bytes)
    }

    #[test]
    fn receive_file_writes_full_stream_atomically_with_mode() {
        let dir = test_dir("full");
        let dest = dir.join("out.bin");
        let data = b"complete payload\x00\xff\x01";
        let mut src = Cursor::new(data.to_vec());

        let n = receive_file(&mut src, &dest, data.len() as u64, 0o640).unwrap();

        assert_eq!(n, data.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        let mode = std::fs::symlink_metadata(&dest)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o640);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn receive_file_rejects_short_stream_and_leaves_no_file() {
        let dir = test_dir("short");
        let dest = dir.join("out.bin");
        let data = b"only a few bytes";
        let mut src = Cursor::new(data.to_vec());

        // Declare more than the source provides — a dropped-connection stand-in.
        let err = receive_file(&mut src, &dest, (data.len() + 100) as u64, 0o644).unwrap_err();

        match err {
            ReceiveError::Incomplete { expected, got } => {
                assert_eq!(expected, (data.len() + 100) as u64);
                assert_eq!(got, data.len() as u64);
            }
            ReceiveError::Io(e) => panic!("expected Incomplete, got Io({e})"),
        }
        assert!(
            !dest.exists(),
            "no file must be installed after a short stream"
        );
        assert!(
            staged_temp_files(&dir).is_empty(),
            "temp files must be cleaned up"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn receive_temp_creation_refuses_an_existing_symlink() {
        let dir = test_dir("symlink");
        let target = dir.join("target");
        std::fs::write(&target, b"do not overwrite").unwrap();
        let tmp = get_temp_path(&dir, "deadbeefdeadbeef");
        std::os::unix::fs::symlink(&target, &tmp).unwrap();

        let err = create_get_temp_with_suffix(&dir, "deadbeefdeadbeef").unwrap_err();

        assert!(
            is_get_temp_name_collision(&err),
            "expected existing symlink to be refused, got {err}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"do not overwrite");
        assert!(
            std::fs::symlink_metadata(&tmp)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn receive_file_ignores_the_old_predictable_temp_name() {
        let dir = test_dir("legacy-symlink");
        let dest = dir.join("out.bin");
        let target = dir.join("target");
        std::fs::write(&target, b"do not overwrite").unwrap();
        let legacy = dir.join(format!(".rxd-get-{}.tmp", std::process::id()));
        std::os::unix::fs::symlink(&target, &legacy).unwrap();
        let data = b"complete payload";
        let mut src = Cursor::new(data.to_vec());

        let n = receive_file(&mut src, &dest, data.len() as u64, 0o600).unwrap();

        assert_eq!(n, data.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(std::fs::read(&target).unwrap(), b"do not overwrite");
        assert!(
            std::fs::symlink_metadata(&legacy)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn receive_file_handles_empty_stream() {
        let dir = test_dir("empty");
        let dest = dir.join("out.bin");
        let mut src = Cursor::new(Vec::new());

        let n = receive_file(&mut src, &dest, 0, 0o644).unwrap();

        assert_eq!(n, 0);
        assert_eq!(std::fs::read(&dest).unwrap(), b"");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // The whole point of staging: a post-receive check can reject a complete
    // payload (rxd reporting the file changed mid-read) and the destination
    // must be left exactly as it was.
    #[test]
    fn discarding_a_staged_file_leaves_the_destination_untouched() {
        let dir = test_dir("discard");
        let dest = dir.join("out.bin");
        std::fs::write(&dest, b"original").unwrap();
        let data = b"replacement payload";
        let mut src = Cursor::new(data.to_vec());

        let staged = receive_to_temp(&mut src, &dest, data.len() as u64).unwrap();
        assert_eq!(staged.bytes, data.len() as u64);
        let tmp = staged.tmp.clone();
        staged.discard();

        assert_eq!(std::fs::read(&dest).unwrap(), b"original");
        assert!(!tmp.exists(), "staged temp must be removed");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn command_hosts_covers_plain_and_host_path_destinations() {
        let run = Command::Run {
            host: "site-router1".to_string(),
            command: vec!["true".to_string()],
            cwd: None,
            env: Vec::new(),
            timeout: Some(30),
            keep_stdin_open: false,
            keep: false,
        };
        assert_eq!(command_hosts(&run), vec!["site-router1"]);

        let put = Command::Put {
            local: "-".to_string(),
            remote: "-oProxyCommand=x:/run/secrets/db".to_string(),
            mode: None,
            owner: None,
            group: None,
            allow_empty: false,
        };
        assert_eq!(command_hosts(&put), vec!["-oProxyCommand=x"]);
        assert!(rem_exec::ssh::validate_host(command_hosts(&put)[0]).is_err());

        // Unsplittable HOST:PATH yields nothing — the command's own
        // "must be HOST:PATH" message is the better error there.
        let malformed = Command::Get {
            remote: "no-colon".to_string(),
            local: "/tmp/x".to_string(),
            mode: None,
        };
        assert!(command_hosts(&malformed).is_empty());

        let deploy = Command::Deploy {
            hosts: vec!["a".to_string(), "b".to_string()],
            binary: None,
            offline: false,
            allow_downgrade: false,
        };
        assert_eq!(command_hosts(&deploy), vec!["a", "b"]);
    }
}
