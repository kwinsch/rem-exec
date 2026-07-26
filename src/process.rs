use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{RemExecError, Result};

/// Return the per-user base directory for process state on the remote side.
///
/// Prefers XDG_RUNTIME_DIR/rem-exec (already per-user, tmpfs, correct perms).
/// Falls back to /tmp/rem-exec-<uid> with ownership/symlink validation.
pub fn remote_base() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(runtime_dir).join("rem-exec");
        return p;
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/rem-exec-{uid}"))
}

/// Validate that the base directory is safe to use: exists, is a real directory
/// (not a symlink), owned by the current user, and mode 0700. Creates it if
/// it doesn't exist.
pub fn ensure_base_dir(base: &std::path::Path) -> Result<()> {
    let uid = unsafe { libc::getuid() };

    if !base.exists() {
        fs::create_dir_all(base)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(base, fs::Permissions::from_mode(0o700))?;
        return Ok(());
    }

    // Validate: use lstat (symlink_metadata) to detect symlinks
    let meta = fs::symlink_metadata(base).map_err(|e| {
        RemExecError::Other(format!("cannot stat base dir {}: {e}", base.display()))
    })?;

    if meta.file_type().is_symlink() {
        return Err(RemExecError::Other(format!(
            "base dir {} is a symlink — refusing to use",
            base.display()
        )));
    }

    if !meta.is_dir() {
        return Err(RemExecError::Other(format!(
            "base dir {} is not a directory",
            base.display()
        )));
    }

    // Check owner
    use std::os::unix::fs::MetadataExt;
    if meta.uid() != uid {
        return Err(RemExecError::Other(format!(
            "base dir {} owned by uid {}, expected {}",
            base.display(),
            meta.uid(),
            uid
        )));
    }

    // Fix permissions to 0700 if group/other bits are set.
    // The directory is owned by us and is not a symlink, so it's safe to chmod.
    if meta.mode() & 0o077 != 0 {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = fs::set_permissions(base, fs::Permissions::from_mode(0o700)) {
            return Err(RemExecError::Other(format!(
                "base dir {} has insecure permissions {:o} and chmod to 0700 failed ({e})",
                base.display(),
                meta.mode() & 0o777
            )));
        }
    }

    Ok(())
}

/// Return true if a user-supplied process ID can only name a managed process.
pub fn is_valid_process_id(id: &str) -> bool {
    id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Per-process state directory layout.
pub struct ProcessDir {
    pub dir: PathBuf,
}

impl ProcessDir {
    pub fn new(base: &Path, id: &str) -> Self {
        Self { dir: base.join(id) }
    }

    pub fn cmd_path(&self) -> PathBuf {
        self.dir.join("cmd")
    }
    pub fn started_path(&self) -> PathBuf {
        self.dir.join("started")
    }
    pub fn ended_path(&self) -> PathBuf {
        self.dir.join("ended")
    }
    pub fn status_path(&self) -> PathBuf {
        self.dir.join("status")
    }
    pub fn pid_path(&self) -> PathBuf {
        self.dir.join("pid")
    }
    pub fn runner_pid_path(&self) -> PathBuf {
        self.dir.join("runner_pid")
    }
    pub fn stdin_pipe_path(&self) -> PathBuf {
        self.dir.join("stdin_pipe")
    }
    pub fn stdin_holder_path(&self) -> PathBuf {
        self.dir.join("stdin_holder_pid")
    }
    pub fn stdout_path(&self) -> PathBuf {
        self.dir.join("stdout")
    }
    pub fn stderr_path(&self) -> PathBuf {
        self.dir.join("stderr")
    }
    /// Marker written by the grandchild when `execvp` fails, holding the raw
    /// errno. It lives in its own file because the runner overwrites `status`
    /// with the generic `exited(127)` after reaping the grandchild.
    pub fn exec_error_path(&self) -> PathBuf {
        self.dir.join("exec_error")
    }

    /// Atomically replace the status file: write a pid-unique temp, then rename
    /// it over `status`. rename(2) is atomic on POSIX, so a concurrent reader
    /// always sees a complete old or new value — never the empty window a plain
    /// truncating write exposes (an empty read parses as `ExitedUnknown` and
    /// would mask a clean exit code).
    pub fn write_status(&self, status: &str) -> std::io::Result<()> {
        let pid = unsafe { libc::getpid() };
        let tmp = self.dir.join(format!("status.{pid}.tmp"));
        fs::write(&tmp, status)?;
        fs::rename(&tmp, self.status_path())
    }

    /// Read the status string from the status file.
    pub fn read_status(&self) -> Result<ProcessState> {
        let s =
            fs::read_to_string(self.status_path()).map_err(|_| RemExecError::ProcessNotFound {
                id: self.id().to_string(),
            })?;
        Ok(ProcessState::parse(s.trim()))
    }

    /// Read the command string.
    pub fn read_cmd(&self) -> Result<String> {
        Ok(fs::read_to_string(self.cmd_path())
            .unwrap_or_default()
            .trim()
            .to_string())
    }

    /// Read the start timestamp.
    pub fn read_started(&self) -> Result<u64> {
        Ok(fs::read_to_string(self.started_path())
            .unwrap_or_default()
            .trim()
            .parse::<u64>()
            .unwrap_or(0))
    }

    /// Read the end timestamp (if process has exited).
    pub fn read_ended(&self) -> Result<Option<u64>> {
        match fs::read_to_string(self.ended_path()) {
            Ok(s) => Ok(s.trim().parse::<u64>().ok()),
            Err(_) => Ok(None),
        }
    }

    /// Read the command PID.
    pub fn read_pid(&self) -> Result<Option<u32>> {
        match fs::read_to_string(self.pid_path()) {
            Ok(s) => Ok(s.trim().parse::<u32>().ok()),
            Err(_) => Ok(None),
        }
    }

    /// Read the stdin holder PID.
    pub fn read_stdin_holder_pid(&self) -> Result<Option<u32>> {
        match fs::read_to_string(self.stdin_holder_path()) {
            Ok(s) => Ok(s.trim().parse::<u32>().ok()),
            Err(_) => Ok(None),
        }
    }

    /// Read the runner PID.
    pub fn read_runner_pid(&self) -> Result<Option<u32>> {
        match fs::read_to_string(self.runner_pid_path()) {
            Ok(s) => Ok(s.trim().parse::<u32>().ok()),
            Err(_) => Ok(None),
        }
    }

    /// Read the recorded exec-failure errno, if the command never started.
    pub fn read_exec_error(&self) -> Option<i32> {
        fs::read_to_string(self.exec_error_path())
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
    }

    /// Extract the process ID from the directory path.
    pub fn id(&self) -> &str {
        self.dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
    }

    /// Get file size for stdout or stderr.
    pub fn stream_size(&self, stream: &str) -> u64 {
        let path = match stream {
            "stdout" => self.stdout_path(),
            "stderr" => self.stderr_path(),
            _ => return 0,
        };
        fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }
}

/// Process state parsed from the status file.
#[derive(Debug, Clone)]
pub enum ProcessState {
    Running,
    /// Exited normally with this code.
    Exited(i32),
    /// Terminated by this signal number.
    Signaled(i32),
    /// Terminated by our own `kill`.
    ExitedKilled,
    /// Died without a recorded exit status (detected by self-healing).
    ExitedUnknown,
    /// The command never started because `execvp` failed; carries the errno.
    ExecFailed(i32),
}

/// Map an exec-failure errno to a stable, agent-branchable reason token.
/// Known cases get a name; anything else falls back to `errno_<n>`.
pub fn exec_reason(errno: i32) -> String {
    match errno {
        libc::ENOENT => "command_not_found".to_string(),
        libc::EACCES => "permission_denied".to_string(),
        libc::ENOEXEC => "exec_format_error".to_string(),
        other => format!("errno_{other}"),
    }
}

impl ProcessState {
    pub fn parse(s: &str) -> Self {
        match s {
            "running" => ProcessState::Running,
            "exited(killed)" => ProcessState::ExitedKilled,
            "exited(unknown)" => ProcessState::ExitedUnknown,
            _ if s.starts_with("signaled(") && s.ends_with(')') => {
                let sig = s["signaled(".len()..s.len() - 1]
                    .parse::<i32>()
                    .unwrap_or(-1);
                ProcessState::Signaled(sig)
            }
            _ if s.starts_with("exited(") && s.ends_with(')') => {
                let code = s[7..s.len() - 1].parse::<i32>().unwrap_or(-1);
                ProcessState::Exited(code)
            }
            _ => ProcessState::ExitedUnknown,
        }
    }
}

impl fmt::Display for ProcessState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcessState::Running => write!(f, "running"),
            ProcessState::Exited(code) => write!(f, "exited({code})"),
            ProcessState::Signaled(sig) => write!(f, "signaled({sig})"),
            ProcessState::ExitedKilled => write!(f, "exited(killed)"),
            ProcessState::ExitedUnknown => write!(f, "exited(unknown)"),
            ProcessState::ExecFailed(errno) => write!(f, "exec_failed({})", exec_reason(*errno)),
        }
    }
}

/// Return the current unix timestamp.
pub fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Generate a random 8-character hex process ID.
pub fn generate_id() -> Result<String> {
    let mut buf = [0u8; 4];
    let bytes_read = {
        use std::io::Read;
        let mut f = fs::File::open("/dev/urandom")?;
        f.read(&mut buf)?
    };
    assert_eq!(bytes_read, 4, "/dev/urandom returned fewer than 4 bytes");
    Ok(hex_encode(&buf))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_path(name: &str) -> PathBuf {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rem-exec-process-test-{}-{id}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn process_state_parses_known_and_unknown_values() {
        assert!(matches!(
            ProcessState::parse("running"),
            ProcessState::Running
        ));
        assert!(matches!(
            ProcessState::parse("exited(0)"),
            ProcessState::Exited(0)
        ));
        assert!(matches!(
            ProcessState::parse("exited(137)"),
            ProcessState::Exited(137)
        ));
        assert!(matches!(
            ProcessState::parse("exited(killed)"),
            ProcessState::ExitedKilled
        ));
        assert!(matches!(
            ProcessState::parse("exited(unknown)"),
            ProcessState::ExitedUnknown
        ));
        assert!(matches!(
            ProcessState::parse("exited(not-a-code)"),
            ProcessState::Exited(-1)
        ));
        assert!(matches!(
            ProcessState::parse("nonsense"),
            ProcessState::ExitedUnknown
        ));
    }

    #[test]
    fn process_id_validation_rejects_path_components() {
        assert!(is_valid_process_id("0123abcd"));
        assert!(is_valid_process_id("ABCDEF09"));

        for id in [
            "",
            "abc",
            "../abcd",
            "/tmp/x",
            "abc/def0",
            "0123abcd9",
            "gggggggg",
        ] {
            assert!(!is_valid_process_id(id), "{id} should be rejected");
        }
    }

    #[test]
    fn ensure_base_dir_creates_private_directory() {
        let base = temp_path("create");

        ensure_base_dir(&base).unwrap();

        let meta = fs::symlink_metadata(&base).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);

        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn ensure_base_dir_fixes_group_and_other_permissions() {
        let base = temp_path("perms");
        fs::create_dir_all(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o755)).unwrap();

        ensure_base_dir(&base).unwrap();

        let mode = fs::symlink_metadata(&base).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);

        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn ensure_base_dir_rejects_symlink() {
        let target = temp_path("target");
        let link = temp_path("link");
        fs::create_dir_all(&target).unwrap();
        symlink(&target, &link).unwrap();

        let err = ensure_base_dir(&link).unwrap_err().to_string();

        assert!(err.contains("is a symlink"), "{err}");

        fs::remove_file(link).unwrap();
        fs::remove_dir_all(target).unwrap();
    }

    #[test]
    fn ensure_base_dir_rejects_plain_file() {
        let base = temp_path("file");
        fs::write(&base, b"not a directory").unwrap();

        let err = ensure_base_dir(&base).unwrap_err().to_string();

        assert!(err.contains("is not a directory"), "{err}");

        fs::remove_file(base).unwrap();
    }

    #[test]
    fn write_status_is_atomic_under_concurrent_reads() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        // A plain truncate+write status update lets a concurrent reader observe
        // an empty file, which ProcessState::parse decodes as ExitedUnknown —
        // masking a real exit code. write_status renames into place, so a reader
        // only ever sees a value that was actually written. This test fails if
        // write_status is made non-atomic.
        let base = temp_path("atomic-status");
        let pdir = ProcessDir::new(&base, "0badf00d");
        fs::create_dir_all(&pdir.dir).unwrap();
        pdir.write_status("running").unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = Arc::clone(&stop);
        let writer_dir = base.clone();
        let writer = std::thread::spawn(move || {
            let wp = ProcessDir::new(&writer_dir, "0badf00d");
            let mut i = 0u64;
            while !writer_stop.load(Ordering::Relaxed) {
                let s = if i % 2 == 0 { "running" } else { "exited(7)" };
                wp.write_status(s).unwrap();
                i += 1;
            }
        });

        // Every observed state must be one we actually wrote — never
        // ExitedUnknown, which is what a torn/empty read decodes to.
        for n in 0..100_000u64 {
            match pdir.read_status().unwrap() {
                ProcessState::Running | ProcessState::Exited(7) => {}
                other => {
                    stop.store(true, Ordering::Relaxed);
                    let _ = writer.join();
                    let _ = fs::remove_dir_all(&base);
                    panic!("torn read observed unwritten state {other:?} after {n} reads");
                }
            }
        }

        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        fs::remove_dir_all(base).unwrap();
    }
}
