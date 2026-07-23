use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crate::process::{ProcessDir, ProcessState, is_valid_process_id, remote_base, unix_timestamp};
use crate::protocol::{ErrorCode, ProcessSummary, Response};
use crate::remote::start;
use crate::{Encoding, encode_bytes};

const WRITE_STDIN_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-stream cap on output inlined into a `run` response. Larger output is
/// tail-truncated here and remains fully readable via `read`.
pub const RUN_INLINE_CAP: u64 = 256 * 1024;

/// Per-stream cap under which an ephemeral `run` may delete the process dir.
///
/// Deliberately much smaller than [`RUN_INLINE_CAP`]: a `completed` response can
/// carry up to 256 KiB, but an agent's tool-output window typically truncates
/// well before that. If we deleted the dir whenever output merely fit inline,
/// mid-band output (bigger than the agent sees, smaller than the inline cap)
/// would be irretrievable — the agent would have to re-run the command, the
/// exact failure mode this tool exists to remove. So we only delete when the
/// output is small enough to be seen in full; anything larger keeps its dir so
/// `rx stdout/stderr --offset` can re-page it.
pub const RUN_EPHEMERAL_CAP: u64 = 16 * 1024;

/// Default timeout for `run` when the caller doesn't specify one.
pub const RUN_DEFAULT_TIMEOUT_MS: u64 = 30_000;

fn invalid_process_id(id: &str) -> Response {
    Response::error_code(ErrorCode::InvalidProcessId, format!("invalid process ID: {id}"))
}

/// Structured (exit_code, signal) from a terminal process state.
fn exit_fields(state: &ProcessState) -> (Option<i32>, Option<i32>) {
    match state {
        ProcessState::Exited(code) => (Some(*code), None),
        ProcessState::Signaled(sig) => (None, Some(*sig)),
        ProcessState::Running
        | ProcessState::ExitedKilled
        | ProcessState::ExitedUnknown
        | ProcessState::ExecFailed(_) => (None, None),
    }
}

/// Read the process state, self-healing a stale "running" status when the
/// process is actually dead. Returns None if the process has no status file.
fn resolve_state(pdir: &ProcessDir) -> Option<ProcessState> {
    let state = pdir.read_status().ok()?;
    if !matches!(state, ProcessState::Running) {
        // A recorded exec failure overrides the runner's generic exited(127):
        // the command never actually started.
        if let Some(errno) = pdir.read_exec_error() {
            return Some(ProcessState::ExecFailed(errno));
        }
        return Some(state);
    }

    let pid = pdir.read_pid().ok().flatten();
    let runner = pdir.read_runner_pid().ok().flatten();

    // No PID recorded yet: the status file is written before the fork, so the
    // process may still be starting. Treat as running so a concurrent
    // list/clean can't mark and remove a newborn process dir.
    if pid.is_none() && runner.is_none() {
        return Some(ProcessState::Running);
    }

    // Status says running — verify the command is actually alive.
    let cmd_alive = pid.is_some_and(|p| unsafe { libc::kill(p as i32, 0) } == 0);
    if cmd_alive {
        return Some(ProcessState::Running);
    }

    // Command is gone. If the runner is still around it is mid-teardown
    // (writing the real exit status), so leave the state as running.
    let runner_alive = runner.is_some_and(|rp| unsafe { libc::kill(rp as i32, 0) } == 0);
    if runner_alive {
        Some(ProcessState::Running)
    } else {
        let _ = pdir.write_status("exited(unknown)");
        let _ = fs::write(pdir.ended_path(), unix_timestamp().to_string());
        Some(ProcessState::ExitedUnknown)
    }
}

/// Get process status with self-healing (see [`resolve_state`]).
pub fn status(id: &str) -> Response {
    if !is_valid_process_id(id) {
        return invalid_process_id(id);
    }

    let base = remote_base();
    let pdir = ProcessDir::new(&base, id);
    let state = match resolve_state(&pdir) {
        Some(s) => s,
        None => {
            return Response::error_code(
                ErrorCode::ProcessNotFound,
                format!("process not found: {id}"),
            );
        }
    };

    let (exit_code, signal) = exit_fields(&state);
    Response::Status {
        id: id.to_string(),
        state: state.to_string(),
        cmd: pdir.read_cmd().unwrap_or_default(),
        started: pdir.read_started().unwrap_or(0),
        ended: pdir.read_ended().unwrap_or(None),
        exit_code,
        signal,
        stdout_size: pdir.stream_size("stdout"),
        stderr_size: pdir.stream_size("stderr"),
    }
}

/// Default read limit: 1 MiB.
const DEFAULT_READ_LIMIT: u64 = 1024 * 1024;

/// Read process output (stdout or stderr) with optional byte offset and limit.
pub fn read_output(id: &str, stream: &str, offset: Option<u64>, limit: Option<u64>) -> Response {
    if !is_valid_process_id(id) {
        return invalid_process_id(id);
    }

    let base = remote_base();
    let pdir = ProcessDir::new(&base, id);
    let path = match stream {
        "stdout" => pdir.stdout_path(),
        "stderr" => pdir.stderr_path(),
        _ => return Response::error_code(ErrorCode::BadRequest, format!("invalid stream: {stream}")),
    };

    let file_size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

    let mut file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => {
            return Response::error_code(
                ErrorCode::ProcessNotFound,
                format!("process not found: {id}"),
            );
        }
    };

    let offset = offset.unwrap_or(0);
    if offset > 0
        && let Err(e) = file.seek(SeekFrom::Start(offset))
    {
        return Response::error_code(ErrorCode::Internal, format!("seek failed: {e}"));
    }

    let limit = limit.unwrap_or(DEFAULT_READ_LIMIT);
    let mut data = vec![0u8; limit.min(file_size.saturating_sub(offset)) as usize];
    let bytes_read = match file.read(&mut data) {
        Ok(n) => n,
        Err(e) => return Response::error_code(ErrorCode::Internal, format!("read failed: {e}")),
    };
    data.truncate(bytes_read);

    let (data, encoding) = encode_bytes(&data);
    Response::Output {
        data,
        encoding,
        offset,
        size: file_size,
    }
}

/// Get the byte size of a stream file.
pub fn size(id: &str, stream: &str) -> Response {
    if !is_valid_process_id(id) {
        return invalid_process_id(id);
    }

    let base = remote_base();
    let pdir = ProcessDir::new(&base, id);
    let sz = pdir.stream_size(stream);
    Response::Output {
        data: String::new(),
        encoding: Encoding::Utf8,
        offset: 0,
        size: sz,
    }
}

/// Write exact bytes to the process's stdin FIFO. Newline handling, if any, is
/// applied by the caller. Uses O_NONBLOCK so a dead process fails fast.
pub fn write_stdin(id: &str, data: &[u8]) -> Response {
    if !is_valid_process_id(id) {
        return invalid_process_id(id);
    }

    let base = remote_base();
    let pdir = ProcessDir::new(&base, id);

    if !pdir.dir.exists() {
        return Response::error_code(ErrorCode::ProcessNotFound, format!("process not found: {id}"));
    }

    if let Ok(state) = pdir.read_status()
        && !matches!(state, ProcessState::Running)
    {
        return Response::error_code(
            ErrorCode::ProcessExited,
            format!("process already exited: {id}"),
        );
    }

    match feed_fifo(&pdir, data) {
        Ok(bytes) => Response::Written { bytes },
        Err(err) if err.kind() == std::io::ErrorKind::TimedOut => {
            Response::error_code(ErrorCode::Timeout, format!("write to stdin timed out: {err}"))
        }
        Err(err) => Response::error_code(ErrorCode::Internal, format!("write failed: {err}")),
    }
}

/// Open the process's stdin FIFO and write `data`, applying backpressure with a
/// timeout. Non-blocking open so a process with no reader fails fast.
fn feed_fifo(pdir: &ProcessDir, data: &[u8]) -> std::io::Result<usize> {
    let fifo = pdir.stdin_pipe_path();
    let fifo_cstr = std::ffi::CString::new(fifo.to_str().unwrap_or(""))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let fd = unsafe { libc::open(fifo_cstr.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let result = write_all_nonblocking(fd, data);
    unsafe { libc::close(fd) };
    result
}

/// Run a command to completion, blocking up to `timeout_ms` (default
/// [`RUN_DEFAULT_TIMEOUT_MS`]). If it outlives the timeout it stays running
/// detached and a `Running` handle is returned instead of blocking forever.
///
/// `body` is fed to the process's stdin. Unless `keep_stdin_open` is set, stdin
/// is closed afterward so commands that read to EOF (cat, sort, sh) terminate.
///
/// When `ephemeral` is true (default), a fully-inlined `completed` response
/// also removes the process directory so short runs do not accumulate state.
/// Truncated output and `running` handles always keep the directory.
pub fn run(
    command: &[String],
    cwd: Option<&str>,
    env: &BTreeMap<String, String>,
    timeout_ms: Option<u64>,
    body: &[u8],
    keep_stdin_open: bool,
    ephemeral: bool,
) -> Response {
    if command.is_empty() {
        return Response::error_code(ErrorCode::BadRequest, "run: empty command");
    }

    let run_started = Instant::now();
    let id = match start::start(command, cwd, env) {
        Ok(Response::Started { id }) => id,
        Ok(other) => return other, // start already returned an error response
        Err(e) => return Response::error_code(ErrorCode::Internal, e.to_string()),
    };

    let base = remote_base();
    let pdir = ProcessDir::new(&base, &id);

    // Feed stdin. Ignore feed errors — the command may not read stdin at all.
    if !body.is_empty() {
        let _ = feed_fifo(&pdir, body);
    }
    if !keep_stdin_open {
        let _ = close_stdin(&id); // send EOF so readers terminate
    }

    let timeout = Duration::from_millis(timeout_ms.unwrap_or(RUN_DEFAULT_TIMEOUT_MS));
    match await_exit_or_timeout(&pdir, timeout) {
        Some(state) => {
            let resp = completed_response(
                &id,
                &pdir,
                &state,
                run_started.elapsed().as_millis() as u64,
            );
            maybe_ephemeral_clean(&pdir, &resp, ephemeral);
            resp
        }
        None => running_response(&id, &pdir, timeout),
    }
}

/// Remove a fully-inlined completed process dir when `ephemeral` is set and the
/// output is small enough to have been seen in full. Must run after the response
/// has been built (reads the stream files first).
///
/// Kept (not deleted) when output is truncated OR larger than
/// [`RUN_EPHEMERAL_CAP`] — in both cases the agent may not have the whole output
/// and needs `rx stdout/stderr --offset` to re-page it rather than re-running.
fn maybe_ephemeral_clean(pdir: &ProcessDir, resp: &Response, ephemeral: bool) {
    if !ephemeral {
        return;
    }
    let Response::Completed {
        stdout_truncated,
        stderr_truncated,
        stdout_size,
        stderr_size,
        ..
    } = resp
    else {
        return;
    };
    if *stdout_truncated || *stderr_truncated {
        return;
    }
    if *stdout_size > RUN_EPHEMERAL_CAP || *stderr_size > RUN_EPHEMERAL_CAP {
        return;
    }
    remove_process_dir(pdir);
}

/// Tear down any leftover helper PIDs and delete the process directory.
fn remove_process_dir(pdir: &ProcessDir) {
    if let Ok(Some(rpid)) = pdir.read_runner_pid() {
        unsafe { libc::kill(rpid as i32, libc::SIGTERM) };
    }
    if let Ok(Some(hpid)) = pdir.read_stdin_holder_pid() {
        unsafe { libc::kill(hpid as i32, libc::SIGTERM) };
    }
    let _ = fs::remove_dir_all(&pdir.dir);
}

/// Block until an already-started process exits or `timeout_ms` elapses.
/// Returns `Completed` on exit, `Running` on timeout — same shapes as `run`,
/// so the async (`start`) path never needs client-side polling.
pub fn wait(id: &str, timeout_ms: Option<u64>) -> Response {
    if !is_valid_process_id(id) {
        return invalid_process_id(id);
    }
    let base = remote_base();
    let pdir = ProcessDir::new(&base, id);
    if resolve_state(&pdir).is_none() {
        return Response::error_code(ErrorCode::ProcessNotFound, format!("process not found: {id}"));
    }

    let timeout = Duration::from_millis(timeout_ms.unwrap_or(RUN_DEFAULT_TIMEOUT_MS));
    match await_exit_or_timeout(&pdir, timeout) {
        Some(state) => completed_response(id, &pdir, &state, process_duration_ms(&pdir)),
        None => running_response(id, &pdir, timeout),
    }
}

/// Poll until the process leaves the running state, or the timeout elapses.
/// Returns the terminal state, or None on timeout.
fn await_exit_or_timeout(pdir: &ProcessDir, timeout: Duration) -> Option<ProcessState> {
    let deadline = Instant::now() + timeout;
    let poll = Duration::from_millis(20);
    loop {
        if let Some(state) = resolve_state(pdir)
            && !matches!(state, ProcessState::Running)
        {
            return Some(state);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(poll);
    }
}

fn running_response(id: &str, pdir: &ProcessDir, timeout: Duration) -> Response {
    Response::Running {
        id: id.to_string(),
        reason: format!("still running after {}ms", timeout.as_millis()),
        stdout_size: pdir.stream_size("stdout"),
        stderr_size: pdir.stream_size("stderr"),
        hint: format!(
            "backgrounded; wait: rx wait HOST {id} · read: rx stdout HOST {id} · stop: rx kill HOST {id}"
        ),
    }
}

/// Process wall time from its recorded start/end (second resolution).
fn process_duration_ms(pdir: &ProcessDir) -> u64 {
    let started = pdir.read_started().unwrap_or(0);
    let ended = pdir.read_ended().unwrap_or(None).unwrap_or(started);
    ended.saturating_sub(started).saturating_mul(1000)
}

fn completed_response(
    id: &str,
    pdir: &ProcessDir,
    state: &ProcessState,
    duration_ms: u64,
) -> Response {
    let (exit_code, signal) = exit_fields(state);
    let exec_error = match state {
        ProcessState::ExecFailed(errno) => Some(crate::process::exec_reason(*errno)),
        _ => None,
    };
    let (stdout, stdout_encoding, stdout_size, stdout_truncated) = read_tail(pdir, "stdout");
    let (stderr, stderr_encoding, stderr_size, stderr_truncated) = read_tail(pdir, "stderr");
    Response::Completed {
        id: id.to_string(),
        exit_code,
        signal,
        exec_error,
        duration_ms,
        stdout,
        stdout_encoding,
        stderr,
        stderr_encoding,
        stdout_size,
        stderr_size,
        stdout_truncated,
        stderr_truncated,
    }
}

/// Read up to the last [`RUN_INLINE_CAP`] bytes of a stream — the tail, where
/// errors and results land. Returns (data, encoding, total_size, truncated).
fn read_tail(pdir: &ProcessDir, stream: &str) -> (String, Encoding, u64, bool) {
    let path = match stream {
        "stdout" => pdir.stdout_path(),
        _ => pdir.stderr_path(),
    };
    let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let (start_at, truncated) = if size > RUN_INLINE_CAP {
        (size - RUN_INLINE_CAP, true)
    } else {
        (0, false)
    };
    let mut file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return (String::new(), Encoding::Utf8, size, false),
    };
    if start_at > 0 {
        let _ = file.seek(SeekFrom::Start(start_at));
    }
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);
    let (data, encoding) = encode_bytes(&buf);
    (data, encoding, size, truncated)
}

/// Write the request body to `path` atomically: stream to a temp file in the
/// target directory, apply mode then owner/group, fsync, and rename into place.
/// The final file only ever appears with its intended permissions — there is no
/// window where it sits at the destination path with default perms.
pub fn put<R: Read>(
    reader: &mut R,
    path: &str,
    mode: Option<u32>,
    owner: Option<&str>,
    group: Option<&str>,
) -> Response {
    let target = Path::new(path);
    if target.file_name().is_none() {
        return Response::error_code(ErrorCode::BadRequest, format!("invalid target path: {path}"));
    }
    let dir = match target.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => Path::new(".").to_path_buf(),
    };

    let suffix = crate::process::generate_id().unwrap_or_else(|_| std::process::id().to_string());
    let tmp = dir.join(format!(".rxd-put-{suffix}.tmp"));

    // Stream body → temp file (constant memory), created private (0600).
    let bytes = {
        let mut f = match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(f) => f,
            Err(e) => {
                return Response::error_code(
                    ErrorCode::Internal,
                    format!("create temp file in {}: {e}", dir.display()),
                );
            }
        };
        match std::io::copy(reader, &mut f) {
            Ok(n) => {
                let _ = f.sync_all();
                n
            }
            Err(e) => {
                drop(f);
                let _ = fs::remove_file(&tmp);
                return Response::error_code(ErrorCode::Internal, format!("write failed: {e}"));
            }
        }
    };

    if let Some(m) = mode
        && let Err(e) = fs::set_permissions(&tmp, fs::Permissions::from_mode(m))
    {
        let _ = fs::remove_file(&tmp);
        return Response::error_code(ErrorCode::Internal, format!("chmod failed: {e}"));
    }

    if (owner.is_some() || group.is_some())
        && let Err(msg) = apply_chown(&tmp, owner, group)
    {
        let _ = fs::remove_file(&tmp);
        return Response::error_code(ErrorCode::Unsupported, msg)
            .with_hint("owner/group need a privileged rxd (e.g. deploy+invoke rxd via doas)");
    }

    if let Err(e) = fs::rename(&tmp, target) {
        let _ = fs::remove_file(&tmp);
        return Response::error_code(ErrorCode::Internal, format!("rename to {path}: {e}"));
    }

    Response::Copied {
        path: path.to_string(),
        bytes,
        mode,
    }
}

/// chown a path by user/group name-or-id. Either may be None (left unchanged).
fn apply_chown(path: &Path, owner: Option<&str>, group: Option<&str>) -> Result<(), String> {
    let uid = match owner {
        Some(o) => Some(resolve_uid(o).ok_or_else(|| format!("unknown user: {o}"))?),
        None => None,
    };
    let gid = match group {
        Some(g) => Some(resolve_gid(g).ok_or_else(|| format!("unknown group: {g}"))?),
        None => None,
    };
    let cpath = std::ffi::CString::new(path.to_str().unwrap_or(""))
        .map_err(|e| format!("invalid path: {e}"))?;
    // (uid_t)-1 / (gid_t)-1 means "do not change".
    let rc = unsafe { libc::chown(cpath.as_ptr(), uid.unwrap_or(u32::MAX), gid.unwrap_or(u32::MAX)) };
    if rc != 0 {
        return Err(format!("chown failed: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}

fn resolve_uid(owner: &str) -> Option<u32> {
    if let Ok(n) = owner.parse::<u32>() {
        return Some(n);
    }
    let c = std::ffi::CString::new(owner).ok()?;
    let pw = unsafe { libc::getpwnam(c.as_ptr()) };
    if pw.is_null() {
        None
    } else {
        Some(unsafe { (*pw).pw_uid })
    }
}

fn resolve_gid(group: &str) -> Option<u32> {
    if let Ok(n) = group.parse::<u32>() {
        return Some(n);
    }
    let c = std::ffi::CString::new(group).ok()?;
    let gr = unsafe { libc::getgrnam(c.as_ptr()) };
    if gr.is_null() {
        None
    } else {
        Some(unsafe { (*gr).gr_gid })
    }
}

fn write_all_nonblocking(fd: libc::c_int, data: &[u8]) -> std::io::Result<usize> {
    let started = Instant::now();
    let mut offset = 0;

    while offset < data.len() {
        let written = unsafe {
            libc::write(
                fd,
                data[offset..].as_ptr() as *const libc::c_void,
                data.len() - offset,
            )
        };

        if written > 0 {
            offset += written as usize;
            continue;
        }

        if written == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "stdin pipe write returned zero bytes",
            ));
        }

        let err = std::io::Error::last_os_error();
        match err.kind() {
            std::io::ErrorKind::Interrupted => {}
            std::io::ErrorKind::WouldBlock => {
                if started.elapsed() >= WRITE_STDIN_TIMEOUT {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "timed out writing to stdin pipe",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
            _ => return Err(err),
        }
    }

    Ok(offset)
}

/// Kill the stdin holder process to signal EOF to the command.
/// Returns true if the holder was alive and killed, false otherwise.
fn kill_stdin_holder(pdir: &ProcessDir) -> bool {
    if let Ok(Some(holder_pid)) = pdir.read_stdin_holder_pid() {
        let alive = unsafe { libc::kill(holder_pid as i32, 0) } == 0;
        if alive {
            unsafe { libc::kill(holder_pid as i32, libc::SIGTERM) };
            return true;
        }
    }
    false
}

/// Close the stdin FIFO (send EOF to the process).
///
/// Kills the holder process, which is the last writer on the FIFO. Once the
/// holder dies, the command sees EOF on stdin.
pub fn close_stdin(id: &str) -> Response {
    if !is_valid_process_id(id) {
        return invalid_process_id(id);
    }

    let base = remote_base();
    let pdir = ProcessDir::new(&base, id);

    if !pdir.dir.exists() {
        return Response::error_code(ErrorCode::ProcessNotFound, format!("process not found: {id}"));
    }

    // Don't touch exited processes
    if let Ok(state) = pdir.read_status()
        && !matches!(state, ProcessState::Running)
    {
        return Response::error_code(
            ErrorCode::ProcessExited,
            format!("process already exited: {id}"),
        );
    }

    if kill_stdin_holder(&pdir) {
        Response::Written { bytes: 0 } // 0 bytes = EOF marker
    } else if resolve_state(&pdir).is_some_and(|s| !matches!(s, ProcessState::Running)) {
        // Lost the race with the process exiting between the status check
        // above and killing the holder.
        Response::error_code(
            ErrorCode::ProcessExited,
            format!("process already exited: {id}"),
        )
    } else {
        Response::error_code(
            ErrorCode::Internal,
            "stdin already closed or no holder found",
        )
    }
}

/// Pipe data from own stdin (SSH channel) to a process's stdin FIFO.
/// The reverse of `follow`: reads stdin in chunks, writes to FIFO.
/// No JSON output — this is a raw data channel.
pub fn pipe_stdin(id: &str, no_close: bool) {
    if !is_valid_process_id(id) {
        std::process::exit(1);
    }

    let base = remote_base();
    let pdir = ProcessDir::new(&base, id);

    if !pdir.dir.exists() {
        std::process::exit(1);
    }

    if let Ok(state) = pdir.read_status()
        && !matches!(state, ProcessState::Running)
    {
        std::process::exit(1);
    }

    let fifo = pdir.stdin_pipe_path();
    let fifo_cstr = match std::ffi::CString::new(fifo.to_str().unwrap_or("")) {
        Ok(c) => c,
        Err(_) => std::process::exit(1),
    };

    // Blocking O_WRONLY — provides backpressure when command's buffer is full
    let fd = unsafe { libc::open(fifo_cstr.as_ptr(), libc::O_WRONLY) };
    if fd < 0 {
        std::process::exit(1);
    }

    // Ignore SIGPIPE — we handle EPIPE from write() return value
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };

    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let mut buf = [0u8; 8192];

    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };

        let mut offset = 0;
        while offset < n {
            let written = unsafe {
                libc::write(
                    fd,
                    buf[offset..n].as_ptr() as *const libc::c_void,
                    n - offset,
                )
            };
            if written <= 0 {
                unsafe { libc::close(fd) };
                if !no_close {
                    kill_stdin_holder(&pdir);
                }
                std::process::exit(1);
            }
            offset += written as usize;
        }
    }

    unsafe { libc::close(fd) };

    if !no_close {
        kill_stdin_holder(&pdir);
    }
}

/// Kill a process with SIGTERM→wait→SIGKILL escalation.
///
/// After setsid(), the runner is the process group leader (PGID == runner_pid).
/// The command and holder inherit that PGID. We kill the entire group.
pub fn kill(id: &str) -> Response {
    if !is_valid_process_id(id) {
        return invalid_process_id(id);
    }

    let base = remote_base();
    let pdir = ProcessDir::new(&base, id);

    if !pdir.dir.exists() {
        return Response::error_code(ErrorCode::ProcessNotFound, format!("process not found: {id}"));
    }

    // Don't overwrite a real exit code with "exited(killed)"
    if let Ok(state) = pdir.read_status()
        && !matches!(state, ProcessState::Running)
    {
        return Response::error_code(
            ErrorCode::ProcessExited,
            format!("process already exited: {id}"),
        );
    }

    let rpid = pdir.read_runner_pid().ok().flatten();
    let cpid = pdir.read_pid().ok().flatten();

    // Phase 1: SIGTERM to process group
    let sent = if let Some(rpid) = rpid {
        (unsafe { libc::kill(-(rpid as i32), libc::SIGTERM) }) == 0
    } else if let Some(cpid) = cpid {
        (unsafe { libc::kill(cpid as i32, libc::SIGTERM) }) == 0
    } else {
        false
    };

    if !sent {
        // Process already dead — just update state
        let _ = pdir.write_status("exited(killed)");
        let _ = fs::write(pdir.ended_path(), unix_timestamp().to_string());
        return Response::Killed { id: id.to_string() };
    }

    // Phase 2: wait briefly for graceful exit
    thread::sleep(Duration::from_millis(200));

    // Phase 3: check if still alive, escalate to SIGKILL
    let still_alive = cpid
        .map(|p| unsafe { libc::kill(p as i32, 0) } == 0)
        .unwrap_or(false);

    if still_alive {
        if let Some(rpid) = rpid {
            unsafe { libc::kill(-(rpid as i32), libc::SIGKILL) };
        } else if let Some(cpid) = cpid {
            unsafe { libc::kill(cpid as i32, libc::SIGKILL) };
        }
        thread::sleep(Duration::from_millis(50));
    }

    let _ = pdir.write_status("exited(killed)");
    let _ = fs::write(pdir.ended_path(), unix_timestamp().to_string());

    Response::Killed { id: id.to_string() }
}

/// List all managed processes.
pub fn list() -> Response {
    let base = remote_base();
    let mut processes = Vec::new();

    if let Ok(entries) = fs::read_dir(&base) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            let pdir = ProcessDir::new(&base, &id);
            if !pdir.status_path().exists() {
                continue;
            }
            let state = resolve_state(&pdir)
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let cmd = pdir.read_cmd().unwrap_or_default();
            processes.push(ProcessSummary { id, state, cmd });
        }
    }

    Response::List { processes }
}

/// Clean up exited processes.
pub fn clean() -> Response {
    let base = remote_base();
    let mut removed = Vec::new();

    if let Ok(entries) = fs::read_dir(&base) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            let pdir = ProcessDir::new(&base, &id);
            if !pdir.status_path().exists() {
                continue;
            }

            if let Some(state) = resolve_state(&pdir)
                && !matches!(state, ProcessState::Running)
            {
                remove_process_dir(&pdir);
                removed.push(id);
            }
        }
    }

    Response::Cleaned { removed }
}

/// Follow (tail) a stream file, writing raw bytes to stdout.
/// Streams until the process exits and all output is flushed.
/// If `offset` is provided, seeks to that position first (for resume).
pub fn follow(id: &str, stream: &str, offset: Option<u64>) -> Response {
    if !is_valid_process_id(id) {
        return invalid_process_id(id);
    }

    let base = remote_base();
    let pdir = ProcessDir::new(&base, id);
    let path = match stream {
        "stdout" => pdir.stdout_path(),
        "stderr" => pdir.stderr_path(),
        _ => return Response::error_code(ErrorCode::BadRequest, format!("invalid stream: {stream}")),
    };

    let mut file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => {
            return Response::error_code(
                ErrorCode::ProcessNotFound,
                format!("process not found: {id}"),
            );
        }
    };

    if let Some(off) = offset
        && off > 0
    {
        let _ = file.seek(SeekFrom::Start(off));
    }

    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let mut buf = [0u8; 8192];

    loop {
        match file.read(&mut buf) {
            Ok(0) => {
                if let Ok(state) = pdir.read_status()
                    && !matches!(state, ProcessState::Running)
                {
                    // Final drain
                    while let Ok(n) = file.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        let _ = stdout.write_all(&buf[..n]);
                    }
                    let _ = stdout.flush();
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            Ok(n) => {
                let _ = stdout.write_all(&buf[..n]);
                let _ = stdout.flush();
            }
            Err(_) => break,
        }
    }

    Response::error("follow completed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_all_nonblocking_writes_payload_larger_than_pipe_capacity() {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let read_fd = fds[0];
        let write_fd = fds[1];

        let flags = unsafe { libc::fcntl(write_fd, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(write_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );

        let data = vec![b'x'; 128 * 1024];
        let expected_len = data.len();
        let reader = thread::spawn(move || {
            let mut out = Vec::with_capacity(expected_len);
            let mut buf = [0u8; 4096];

            while out.len() < expected_len {
                let n = unsafe {
                    libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                };
                if n > 0 {
                    out.extend_from_slice(&buf[..n as usize]);
                } else if n == 0 {
                    break;
                } else {
                    let err = std::io::Error::last_os_error();
                    if err.kind() != std::io::ErrorKind::Interrupted {
                        panic!("pipe read failed: {err}");
                    }
                }
            }

            unsafe { libc::close(read_fd) };
            out
        });

        let written = write_all_nonblocking(write_fd, &data).unwrap();
        unsafe { libc::close(write_fd) };

        let out = reader.join().unwrap();
        assert_eq!(written, data.len());
        assert_eq!(out, data);
    }
}
