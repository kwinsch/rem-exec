use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::thread;
use std::time::Duration;

use crate::base64_encode;
use crate::process::{ProcessDir, ProcessState, remote_base, unix_timestamp};
use crate::protocol::{ProcessSummary, Response};

/// Get process status with self-healing: if status says "running" but process
/// is dead, update to "exited(unknown)".
pub fn status(id: &str) -> Response {
    let base = remote_base();
    let pdir = ProcessDir::new(&base, id);
    let state = match pdir.read_status() {
        Ok(s) => s,
        Err(_) => return Response::error(format!("process not found: {id}")),
    };

    // Self-healing: check if process is actually alive
    let state = if matches!(state, ProcessState::Running) {
        if let Ok(Some(pid)) = pdir.read_pid() {
            let alive = unsafe { libc::kill(pid as i32, 0) } == 0;
            if !alive {
                let runner_alive = pdir
                    .read_runner_pid()
                    .ok()
                    .flatten()
                    .is_some_and(|rp| unsafe { libc::kill(rp as i32, 0) } == 0);
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

/// Default read limit: 1 MiB.
const DEFAULT_READ_LIMIT: u64 = 1024 * 1024;

/// Read process output (stdout or stderr) with optional byte offset and limit.
pub fn read_output(id: &str, stream: &str, offset: Option<u64>, limit: Option<u64>) -> Response {
    let base = remote_base();
    let pdir = ProcessDir::new(&base, id);
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
    let base = remote_base();
    let pdir = ProcessDir::new(&base, id);
    let sz = pdir.stream_size(stream);
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
    let base = remote_base();
    let pdir = ProcessDir::new(&base, id);
    let fifo = pdir.stdin_pipe_path();

    if !pdir.dir.exists() {
        return Response::error(format!("process not found: {id}"));
    }

    if let Ok(state) = pdir.read_status() {
        if !matches!(state, ProcessState::Running) {
            return Response::error(format!("process already exited: {id}"));
        }
    }

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

/// Close the stdin FIFO (send EOF to the process).
///
/// Kills the holder process, which is the last writer on the FIFO. Once the
/// holder dies, the command sees EOF on stdin.
pub fn close_stdin(id: &str) -> Response {
    let base = remote_base();
    let pdir = ProcessDir::new(&base, id);

    if !pdir.dir.exists() {
        return Response::error(format!("process not found: {id}"));
    }

    if let Ok(Some(holder_pid)) = pdir.read_stdin_holder_pid() {
        let alive = unsafe { libc::kill(holder_pid as i32, 0) } == 0;
        if alive {
            unsafe { libc::kill(holder_pid as i32, libc::SIGTERM) };
            Response::Written { bytes: 0 } // 0 bytes = EOF marker
        } else {
            Response::error("stdin already closed")
        }
    } else {
        Response::error("no stdin holder found")
    }
}

/// Kill a process with SIGTERM→wait→SIGKILL escalation.
///
/// After setsid(), the runner is the process group leader (PGID == runner_pid).
/// The command and holder inherit that PGID. We kill the entire group.
pub fn kill(id: &str) -> Response {
    let base = remote_base();
    let pdir = ProcessDir::new(&base, id);

    if !pdir.dir.exists() {
        return Response::error(format!("process not found: {id}"));
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
        let _ = fs::write(pdir.status_path(), "exited(killed)");
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

    let _ = fs::write(pdir.status_path(), "exited(killed)");
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

            if let Ok(state) = pdir.read_status() {
                if !matches!(state, ProcessState::Running) {
                    if let Ok(Some(rpid)) = pdir.read_runner_pid() {
                        unsafe { libc::kill(rpid as i32, libc::SIGTERM) };
                    }
                    if let Ok(Some(hpid)) = pdir.read_stdin_holder_pid() {
                        unsafe { libc::kill(hpid as i32, libc::SIGTERM) };
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
/// Streams until the process exits and all output is flushed.
/// If `offset` is provided, seeks to that position first (for resume).
pub fn follow(id: &str, stream: &str, offset: Option<u64>) -> Response {
    let base = remote_base();
    let pdir = ProcessDir::new(&base, id);
    let path = match stream {
        "stdout" => pdir.stdout_path(),
        "stderr" => pdir.stderr_path(),
        _ => return Response::error(format!("invalid stream: {stream}")),
    };

    let mut file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return Response::error(format!("process not found: {id}")),
    };

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
                if let Ok(state) = pdir.read_status() {
                    if !matches!(state, ProcessState::Running) {
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
