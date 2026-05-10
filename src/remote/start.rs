use std::ffi::CString;
use std::fs;
use std::os::unix::io::RawFd;

use crate::error::{RemExecError, Result};
use crate::process::{ProcessDir, generate_id, remote_base, unix_timestamp};
use crate::protocol::Response;

/// Start a new process, detaching it from the current session.
///
/// Architecture (after setsid):
///   Runner (child) ─┬─ forks Holder: keeps FIFO write-end open, pauses
///                    └─ forks Grandchild: exec's the command with FIFO as stdin
///
/// EOF support: killing the Holder closes the last writer on the FIFO,
/// so the command sees EOF on stdin.
pub fn start(command: &[String]) -> Result<Response> {
    assert!(!command.is_empty(), "command must not be empty");

    let id = generate_id()?;
    let base = remote_base();
    let pdir = ProcessDir::new(&base, &id);

    // Create process directory with restrictive permissions
    fs::create_dir_all(&pdir.dir)?;
    // Set directory to 0700
    let dir_cstr = CString::new(pdir.dir.to_str().unwrap()).unwrap();
    unsafe { libc::chmod(dir_cstr.as_ptr(), 0o700) };

    fs::write(pdir.status_path(), "running")?;
    fs::write(pdir.cmd_path(), command.join(" "))?;
    fs::write(pdir.started_path(), unix_timestamp().to_string())?;

    // Create stdout/stderr files (empty, so reads don't fail)
    fs::write(pdir.stdout_path(), "")?;
    fs::write(pdir.stderr_path(), "")?;

    // Create the FIFO for stdin
    let fifo_path = pdir.stdin_pipe_path();
    let fifo_cstr = CString::new(fifo_path.to_str().unwrap()).unwrap();
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

    let pid = unsafe { libc::fork() };
    match pid {
        -1 => Err(RemExecError::ForkFailed(
            std::io::Error::last_os_error().to_string(),
        )),
        0 => {
            // === CHILD (becomes runner) ===
            unsafe { libc::close(sync_read) };
            unsafe { libc::setsid() };

            // Set umask for private files
            unsafe { libc::umask(0o077) };

            // Redirect own stdio to /dev/null so SSH can close
            let devnull = open_devnull(libc::O_RDWR);
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
            assert!(fifo_fd >= 0, "failed to open FIFO O_RDWR");

            // Open stdout/stderr output files (0600 via umask)
            let stdout_path = CString::new(pdir.stdout_path().to_str().unwrap()).unwrap();
            let stderr_path = CString::new(pdir.stderr_path().to_str().unwrap()).unwrap();
            let stdout_fd = unsafe {
                libc::open(
                    stdout_path.as_ptr(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC,
                    0o600,
                )
            };
            let stderr_fd = unsafe {
                libc::open(
                    stderr_path.as_ptr(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC,
                    0o600,
                )
            };
            assert!(stdout_fd >= 0, "failed to open stdout file");
            assert!(stderr_fd >= 0, "failed to open stderr file");

            // --- Fork the FIFO holder ---
            // The holder keeps the FIFO write-end alive. Killing it sends EOF
            // to the command's stdin.
            let holder_pid = unsafe { libc::fork() };
            match holder_pid {
                -1 => {
                    let _ = fs::write(pdir.status_path(), "exited(127)");
                    write_pid_to_pipe(sync_write, 0);
                    unsafe { libc::_exit(1) };
                }
                0 => {
                    // === HOLDER ===
                    // Keep fifo_fd open (inherited O_RDWR), close everything else
                    unsafe { libc::close(sync_write) };
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
                    let _ = fs::write(pdir.status_path(), "exited(127)");
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
                    unsafe { libc::close(fifo_fd) };
                    let stdin_fd = unsafe { libc::open(fifo_cstr.as_ptr(), libc::O_RDONLY) };
                    assert!(stdin_fd >= 0, "failed to open FIFO O_RDONLY for stdin");

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

                    let prog = CString::new(command[0].as_str()).unwrap();
                    let args: Vec<CString> = command
                        .iter()
                        .map(|a| CString::new(a.as_str()).unwrap())
                        .collect();
                    let argv: Vec<*const libc::c_char> = args
                        .iter()
                        .map(|a| a.as_ptr())
                        .chain(std::iter::once(std::ptr::null()))
                        .collect();

                    unsafe { libc::execvp(prog.as_ptr(), argv.as_ptr()) };

                    // exec failed
                    let _ = fs::write(pdir.status_path(), "exited(127)");
                    unsafe { libc::_exit(127) };
                }
                gc_pid => {
                    // === RUNNER — waits for grandchild, manages lifecycle ===
                    // Close our copy of the FIFO fd — only holder keeps it now.
                    // This is critical: when holder is killed, no writers remain
                    // and the grandchild sees EOF.
                    unsafe { libc::close(fifo_fd) };

                    // Report PIDs
                    write_pid_to_pipe(sync_write, gc_pid);
                    unsafe { libc::close(sync_write) };

                    let runner_pid = unsafe { libc::getpid() };
                    let _ = fs::write(pdir.runner_pid_path(), runner_pid.to_string());
                    let _ = fs::write(pdir.stdin_holder_path(), holder_pid.to_string());

                    // Wait for the grandchild to exit
                    let mut wstatus: i32 = 0;
                    unsafe { libc::waitpid(gc_pid, &mut wstatus, 0) };

                    let exit_code = if libc::WIFEXITED(wstatus) {
                        libc::WEXITSTATUS(wstatus)
                    } else if libc::WIFSIGNALED(wstatus) {
                        128 + libc::WTERMSIG(wstatus)
                    } else {
                        -1
                    };

                    let _ = fs::write(pdir.status_path(), format!("exited({exit_code})"));
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
            // === PARENT — read grandchild PID, print JSON, exit ===
            unsafe { libc::close(sync_write) };

            let gc_pid = read_pid_from_pipe(sync_read);
            unsafe { libc::close(sync_read) };

            if gc_pid > 0 {
                let _ = fs::write(pdir.pid_path(), gc_pid.to_string());
            }

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

fn open_devnull(flags: i32) -> RawFd {
    let path = CString::new("/dev/null").unwrap();
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    assert!(fd >= 0, "failed to open /dev/null");
    fd
}
