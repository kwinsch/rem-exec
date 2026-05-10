use std::ffi::CString;
use std::fs;
use std::os::unix::io::RawFd;
use std::path::Path;

use crate::error::{RemExecError, Result};
use crate::process::{ProcessDir, REMOTE_BASE, generate_id, unix_timestamp};
use crate::protocol::Response;

/// Start a new process, detaching it from the current session.
///
/// Uses double-fork + setsid so the process survives SSH disconnect.
/// Parent-child synchronization via pipe eliminates race conditions.
pub fn start(command: &[String]) -> Result<Response> {
    assert!(!command.is_empty(), "command must not be empty");

    let id = generate_id()?;
    let base = Path::new(REMOTE_BASE);
    let pdir = ProcessDir::new(base, &id);

    // Create process directory and state files
    fs::create_dir_all(&pdir.dir)?;
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

    // Create a synchronization pipe: child writes the grandchild PID,
    // parent reads it before printing output.
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
            // === CHILD ===
            // Close read end of sync pipe
            unsafe { libc::close(sync_read) };

            // New session — detach from SSH terminal
            unsafe { libc::setsid() };

            // Redirect own stdin/stdout/stderr to /dev/null so SSH can close
            let devnull = open_devnull(libc::O_RDWR);
            unsafe {
                libc::dup2(devnull, 0);
                libc::dup2(devnull, 1);
                libc::dup2(devnull, 2);
                if devnull > 2 {
                    libc::close(devnull);
                }
            }

            // Open the FIFO with O_RDWR (Linux-specific: non-blocking, prevents deadlock)
            let fifo_fd = unsafe {
                libc::open(fifo_cstr.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC)
            };
            assert!(fifo_fd >= 0, "failed to open FIFO O_RDWR");

            // Open stdout/stderr output files
            let stdout_path = CString::new(pdir.stdout_path().to_str().unwrap()).unwrap();
            let stderr_path = CString::new(pdir.stderr_path().to_str().unwrap()).unwrap();
            let stdout_fd = unsafe {
                libc::open(
                    stdout_path.as_ptr(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC,
                    0o644,
                )
            };
            let stderr_fd = unsafe {
                libc::open(
                    stderr_path.as_ptr(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC,
                    0o644,
                )
            };
            assert!(stdout_fd >= 0, "failed to open stdout file");
            assert!(stderr_fd >= 0, "failed to open stderr file");

            // Second fork: grandchild exec's the command, child becomes the runner
            let grandchild = unsafe { libc::fork() };
            match grandchild {
                -1 => {
                    // Fork failed — write error and exit
                    let _ = fs::write(pdir.status_path(), "exited(127)");
                    write_pid_to_pipe(sync_write, 0);
                    unsafe { libc::_exit(1) };
                }
                0 => {
                    // === GRANDCHILD — will exec the command ===
                    unsafe { libc::close(sync_write) };

                    // Wire up file descriptors: FIFO→stdin, files→stdout/stderr
                    // Clear O_CLOEXEC since we want these to survive exec
                    unsafe {
                        libc::dup2(fifo_fd, 0);
                        libc::dup2(stdout_fd, 1);
                        libc::dup2(stderr_fd, 2);
                        // Close originals if they're not 0/1/2
                        for fd in [fifo_fd, stdout_fd, stderr_fd] {
                            if fd > 2 {
                                libc::close(fd);
                            }
                        }
                    }

                    // exec the command
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
                    // === CHILD (runner) — holds FIFO, waits for grandchild ===
                    // Write grandchild PID to sync pipe so parent can record it
                    write_pid_to_pipe(sync_write, gc_pid);
                    unsafe { libc::close(sync_write) };

                    // Write runner PID
                    let _ = fs::write(pdir.runner_pid_path(), unsafe {
                        libc::getpid().to_string()
                    });

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

                    // Close the FIFO fd (the holder)
                    unsafe { libc::close(fifo_fd) };
                    unsafe { libc::close(stdout_fd) };
                    unsafe { libc::close(stderr_fd) };

                    unsafe { libc::_exit(0) };
                }
            }
        }
        _parent_pid => {
            // === PARENT — read grandchild PID from sync pipe, print response ===
            unsafe { libc::close(sync_write) };

            let gc_pid = read_pid_from_pipe(sync_read);
            unsafe { libc::close(sync_read) };

            // Write the command PID to the process directory
            if gc_pid > 0 {
                let _ = fs::write(pdir.pid_path(), gc_pid.to_string());
            }

            // Don't waitpid the child — it's the runner process (setsid'd) that
            // lives until the command exits. It will be reparented to init when
            // we exit. The SIGCHLD for the zombie is harmless and brief.

            Ok(Response::Started {
                id: id.to_string(),
            })
        }
    }
}

/// Write a PID (as 4 bytes) to a pipe fd.
fn write_pid_to_pipe(fd: RawFd, pid: i32) {
    let bytes = pid.to_ne_bytes();
    unsafe {
        libc::write(fd, bytes.as_ptr() as *const libc::c_void, 4);
    }
}

/// Read a PID (as 4 bytes) from a pipe fd.
fn read_pid_from_pipe(fd: RawFd) -> i32 {
    let mut buf = [0u8; 4];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 4) };
    if n == 4 {
        i32::from_ne_bytes(buf)
    } else {
        0
    }
}

fn open_devnull(flags: i32) -> RawFd {
    let path = CString::new("/dev/null").unwrap();
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    assert!(fd >= 0, "failed to open /dev/null");
    fd
}
