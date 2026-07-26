use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::RawFd;

use crate::error::{RemExecError, Result};
use crate::process::{ProcessDir, generate_id, remote_base, unix_timestamp};
use crate::protocol::{ErrorCode, Response};

/// Start a new process, detaching it from the current session.
///
/// Architecture (after setsid):
///   Runner (child) ─┬─ forks Holder: keeps FIFO write-end open, pauses
///                    └─ forks Grandchild: exec's the command with FIFO as stdin
///
/// EOF support: killing the Holder closes the last writer on the FIFO,
/// so the command sees EOF on stdin.
pub fn start(command: &[String], cwd: Option<&str>, env: &BTreeMap<String, String>) -> Result<Response> {
    assert!(!command.is_empty(), "command must not be empty");

    // Reject NUL bytes up front: an argv NUL would make CString::new().unwrap()
    // panic the grandchild, and a NUL in cwd would silently skip chdir.
    if command.iter().any(|a| a.as_bytes().contains(&0)) {
        return Ok(Response::error_code(
            ErrorCode::BadRequest,
            "command argument contains a NUL byte",
        ));
    }
    if let Some(dir) = cwd
        && dir.as_bytes().contains(&0)
    {
        return Ok(Response::error_code(
            ErrorCode::BadRequest,
            "cwd contains a NUL byte",
        ));
    }
    if env
        .iter()
        .any(|(k, v)| k.as_bytes().contains(&0) || v.as_bytes().contains(&0))
    {
        return Ok(Response::error_code(
            ErrorCode::BadRequest,
            "env key or value contains a NUL byte",
        ));
    }

    let id = generate_id()?;
    let base = remote_base();
    let pdir = ProcessDir::new(&base, &id);

    // Create process directory with restrictive permissions
    fs::create_dir_all(&pdir.dir)?;
    fs::set_permissions(&pdir.dir, fs::Permissions::from_mode(0o700))?;

    pdir.write_status("running")?;
    fs::write(pdir.cmd_path(), command.join(" "))?;
    fs::write(pdir.started_path(), unix_timestamp().to_string())?;

    // Create stdout/stderr files (empty, so reads don't fail)
    fs::write(pdir.stdout_path(), "")?;
    fs::write(pdir.stderr_path(), "")?;
    fs::set_permissions(pdir.stdout_path(), fs::Permissions::from_mode(0o600))?;
    fs::set_permissions(pdir.stderr_path(), fs::Permissions::from_mode(0o600))?;

    // Everything the forked child needs as a C string is built here, before the
    // fork. After fork() the child may not allocate or panic safely — it can
    // inherit locks the parent held — so a failure has to surface as an ordinary
    // error on this side.
    let fifo_path = pdir.stdin_pipe_path();
    let fifo_cstr = path_cstring(&fifo_path)?;
    let stdout_cstr = path_cstring(&pdir.stdout_path())?;
    let stderr_cstr = path_cstring(&pdir.stderr_path())?;
    let prog_cstr = CString::new(command[0].as_str())
        .map_err(|e| RemExecError::Other(format!("command is not a valid C string: {e}")))?;
    let arg_cstrs: Vec<CString> = command
        .iter()
        .map(|a| {
            CString::new(a.as_str())
                .map_err(|e| RemExecError::Other(format!("argument is not a valid C string: {e}")))
        })
        .collect::<Result<_>>()?;

    // Create the FIFO for stdin
    let rc = unsafe { libc::mkfifo(fifo_cstr.as_ptr(), 0o600) };
    if rc != 0 {
        return Err(RemExecError::Io(std::io::Error::last_os_error()));
    }

    // Sync pipe: runner writes grandchild PID, parent reads it.
    let mut sync_pipe = [0i32; 2];
    if unsafe { libc::pipe(sync_pipe.as_mut_ptr()) } != 0 {
        return Err(RemExecError::Io(std::io::Error::last_os_error()));
    }
    let sync_read = sync_pipe[0];
    let sync_write = sync_pipe[1];

    // Ready pipe: the grandchild signals after opening its stdin, so the parent
    // doesn't return — and `run` doesn't feed/close stdin — before a reader is
    // attached to the FIFO. Otherwise buffered input can be discarded when the
    // last fd closes before the command opens the read end.
    let mut ready_pipe = [0i32; 2];
    if unsafe { libc::pipe(ready_pipe.as_mut_ptr()) } != 0 {
        unsafe {
            libc::close(sync_read);
            libc::close(sync_write);
        }
        return Err(RemExecError::Io(std::io::Error::last_os_error()));
    }
    let ready_read = ready_pipe[0];
    let ready_write = ready_pipe[1];

    let pid = unsafe { libc::fork() };
    match pid {
        -1 => Err(RemExecError::ForkFailed(
            std::io::Error::last_os_error().to_string(),
        )),
        0 => {
            // === CHILD (becomes runner) ===
            unsafe { libc::close(sync_read) };
            unsafe { libc::close(ready_read) };
            unsafe { libc::setsid() };

            // Set umask for private files
            unsafe { libc::umask(0o077) };

            // Past this point we are in a forked child: no panicking, no
            // allocation-heavy error paths. A setup failure takes the same exit
            // route the fork-failure arms below use — record the status, release
            // the parent from its read, and _exit. Panicking here would run the
            // panic runtime against locks inherited from the parent, which can
            // hang instead of aborting, and reaches an agent as a bare SIGABRT.
            macro_rules! runner_failed {
                () => {{
                    let _ = pdir.write_status("exited(127)");
                    write_pid_to_pipe(sync_write, 0);
                    unsafe { libc::_exit(1) };
                }};
            }

            // Redirect own stdio to /dev/null so SSH can close
            let devnull = open_devnull(libc::O_RDWR);
            if devnull < 0 {
                runner_failed!();
            }
            unsafe {
                libc::dup2(devnull, 0);
                libc::dup2(devnull, 1);
                libc::dup2(devnull, 2);
                if devnull > 2 {
                    libc::close(devnull);
                }
            }

            // Open FIFO O_RDWR (Linux: non-blocking open, prevents deadlock)
            let fifo_fd = unsafe { libc::open(fifo_cstr.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            if fifo_fd < 0 {
                runner_failed!();
            }

            // Open stdout/stderr output files (0600 via umask)
            let stdout_fd = unsafe {
                libc::open(
                    stdout_cstr.as_ptr(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC,
                    0o600,
                )
            };
            let stderr_fd = unsafe {
                libc::open(
                    stderr_cstr.as_ptr(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if stdout_fd < 0 || stderr_fd < 0 {
                runner_failed!();
            }

            // --- Fork the FIFO holder ---
            // The holder keeps the FIFO write-end alive. Killing it sends EOF
            // to the command's stdin.
            let holder_pid = unsafe { libc::fork() };
            match holder_pid {
                -1 => {
                    let _ = pdir.write_status("exited(127)");
                    write_pid_to_pipe(sync_write, 0);
                    unsafe { libc::_exit(1) };
                }
                0 => {
                    // === HOLDER ===
                    // Keep fifo_fd open (inherited O_RDWR), close everything else
                    unsafe { libc::close(sync_write) };
                    unsafe { libc::close(ready_write) };
                    unsafe { libc::close(stdout_fd) };
                    unsafe { libc::close(stderr_fd) };
                    // Block forever until killed
                    loop {
                        unsafe { libc::pause() };
                    }
                }
                _ => {} // runner continues below
            }

            // --- Fork the command ---
            let grandchild = unsafe { libc::fork() };
            match grandchild {
                -1 => {
                    let _ = pdir.write_status("exited(127)");
                    write_pid_to_pipe(sync_write, 0);
                    unsafe { libc::kill(holder_pid, libc::SIGTERM) };
                    unsafe { libc::_exit(1) };
                }
                0 => {
                    // === GRANDCHILD — exec's the command ===
                    unsafe { libc::close(sync_write) };

                    // Close the inherited O_RDWR fifo fd — it acts as both reader
                    // and writer, which prevents EOF when the holder dies.
                    // Instead, open the FIFO O_RDONLY so we're only a reader.
                    //
                    // Open non-blocking: a blocking O_RDONLY open waits for a
                    // writer, which deadlocks if stdin was closed (holder killed)
                    // before we get here — e.g. `run` closing stdin right after
                    // start. O_RDONLY|O_NONBLOCK always returns immediately; we
                    // then clear O_NONBLOCK so reads block for data / see EOF
                    // normally.
                    unsafe { libc::close(fifo_fd) };
                    let stdin_fd = unsafe {
                        libc::open(fifo_cstr.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK)
                    };
                    if stdin_fd < 0 {
                        // Report on the captured stderr file (not yet dup2'd) and
                        // exit. Dropping ready_write on exit gives the parent EOF,
                        // so it never blocks waiting for a stdin-ready byte.
                        let msg = b"rxd: failed to open stdin FIFO\n";
                        unsafe {
                            libc::write(
                                stderr_fd,
                                msg.as_ptr() as *const libc::c_void,
                                msg.len(),
                            );
                        }
                        let _ = pdir.write_status("exited(127)");
                        unsafe { libc::_exit(127) };
                    }
                    let flags = unsafe { libc::fcntl(stdin_fd, libc::F_GETFL) };
                    if flags >= 0 {
                        unsafe { libc::fcntl(stdin_fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) };
                    }

                    // Signal the parent that stdin is attached — safe now to feed
                    // input / close stdin without losing buffered data.
                    let ready_byte = [1u8];
                    unsafe {
                        libc::write(ready_write, ready_byte.as_ptr() as *const libc::c_void, 1);
                        libc::close(ready_write);
                    }

                    // Wire up: FIFO(read-only)→stdin, files→stdout/stderr
                    unsafe {
                        libc::dup2(stdin_fd, 0);
                        libc::dup2(stdout_fd, 1);
                        libc::dup2(stderr_fd, 2);
                        for fd in [stdin_fd, stdout_fd, stderr_fd] {
                            if fd > 2 {
                                libc::close(fd);
                            }
                        }
                    }

                    // Environment overrides, layered on the inherited env.
                    for (k, v) in env {
                        if let (Ok(ck), Ok(cv)) =
                            (CString::new(k.as_str()), CString::new(v.as_str()))
                        {
                            unsafe { libc::setenv(ck.as_ptr(), cv.as_ptr(), 1) };
                        }
                    }

                    // Working directory (after dup2 so failures reach stderr).
                    if let Some(dir) = cwd
                        && let Ok(cdir) = CString::new(dir)
                        && unsafe { libc::chdir(cdir.as_ptr()) } != 0
                    {
                        let msg = format!(
                            "rxd: chdir({dir}) failed: {}\n",
                            std::io::Error::last_os_error()
                        );
                        unsafe {
                            libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
                        }
                        let _ = pdir.write_status("exited(127)");
                        unsafe { libc::_exit(127) };
                    }

                    let prog = &prog_cstr;
                    let argv: Vec<*const libc::c_char> = arg_cstrs
                        .iter()
                        .map(|a| a.as_ptr())
                        .chain(std::iter::once(std::ptr::null()))
                        .collect();

                    unsafe { libc::execvp(prog.as_ptr(), argv.as_ptr()) };

                    // exec failed — capture errno first (before any libc call
                    // clobbers it), tell the caller why on captured stderr, and
                    // record the errno in a marker file the runner won't touch.
                    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                    let msg = format!(
                        "rx: exec {}: {}\n",
                        command[0],
                        std::io::Error::from_raw_os_error(errno)
                    );
                    unsafe {
                        libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
                    }
                    let _ = fs::write(pdir.exec_error_path(), errno.to_string());
                    let _ = pdir.write_status("exited(127)");
                    unsafe { libc::_exit(127) };
                }
                gc_pid => {
                    // === RUNNER — waits for grandchild, manages lifecycle ===
                    // Close our copy of the FIFO fd — only holder keeps it now.
                    // This is critical: when holder is killed, no writers remain
                    // and the grandchild sees EOF.
                    unsafe { libc::close(fifo_fd) };
                    // Only the grandchild should hold ready_write, so the parent
                    // sees EOF if the grandchild dies before signaling.
                    unsafe { libc::close(ready_write) };

                    // Record the runner/holder PIDs BEFORE signaling the parent.
                    // The parent writes the command's pid file only after reading
                    // this signal, and `run` polls only after that — so recording
                    // runner_pid first guarantees resolve_state sees a runner
                    // whenever it sees the command. Otherwise a command that exits
                    // in the gap between the signal and this write is seen as dead
                    // with no runner recorded, and self-heals to exited(unknown).
                    let runner_pid = unsafe { libc::getpid() };
                    let _ = fs::write(pdir.runner_pid_path(), runner_pid.to_string());
                    let _ = fs::write(pdir.stdin_holder_path(), holder_pid.to_string());

                    // Report PIDs
                    write_pid_to_pipe(sync_write, gc_pid);
                    unsafe { libc::close(sync_write) };

                    // Wait for the grandchild to exit
                    let mut wstatus: i32 = 0;
                    unsafe { libc::waitpid(gc_pid, &mut wstatus, 0) };

                    let status_line = if libc::WIFEXITED(wstatus) {
                        format!("exited({})", libc::WEXITSTATUS(wstatus))
                    } else if libc::WIFSIGNALED(wstatus) {
                        format!("signaled({})", libc::WTERMSIG(wstatus))
                    } else {
                        "exited(unknown)".to_string()
                    };

                    let _ = pdir.write_status(&status_line);
                    let _ = fs::write(pdir.ended_path(), unix_timestamp().to_string());

                    // Clean up holder
                    unsafe { libc::kill(holder_pid, libc::SIGTERM) };
                    unsafe { libc::close(stdout_fd) };
                    unsafe { libc::close(stderr_fd) };

                    unsafe { libc::_exit(0) };
                }
            }
        }
        _parent_pid => {
            // === PARENT — read grandchild PID, wait for stdin-ready, exit ===
            unsafe { libc::close(sync_write) };
            unsafe { libc::close(ready_write) };

            let gc_pid = read_pid_from_pipe(sync_read);
            unsafe { libc::close(sync_read) };

            if gc_pid > 0 {
                let _ = fs::write(pdir.pid_path(), gc_pid.to_string());
                // Block until the grandchild has opened stdin (1 byte) or died
                // (EOF) — after this it is safe to feed or close stdin.
                let mut ready = [0u8; 1];
                unsafe {
                    libc::read(ready_read, ready.as_mut_ptr() as *mut libc::c_void, 1);
                }
            }
            unsafe { libc::close(ready_read) };

            Ok(Response::Started { id: id.to_string() })
        }
    }
}

fn write_pid_to_pipe(fd: RawFd, pid: i32) {
    let bytes = pid.to_ne_bytes();
    unsafe {
        libc::write(fd, bytes.as_ptr() as *const libc::c_void, 4);
    }
}

fn read_pid_from_pipe(fd: RawFd) -> i32 {
    let mut buf = [0u8; 4];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 4) };
    if n == 4 { i32::from_ne_bytes(buf) } else { 0 }
}

/// Build a C string from a path, failing here rather than in a forked child.
fn path_cstring(path: &std::path::Path) -> Result<CString> {
    let s = path.to_str().ok_or_else(|| {
        RemExecError::Other(format!("path is not valid UTF-8: {}", path.display()))
    })?;
    CString::new(s)
        .map_err(|e| RemExecError::Other(format!("path is not a valid C string: {e}")))
}

/// Returns `-1` on failure. Called after `fork()`, where the caller must handle
/// the error itself rather than panic.
fn open_devnull(flags: i32) -> RawFd {
    let path = c"/dev/null";
    unsafe { libc::open(path.as_ptr(), flags) }
}
