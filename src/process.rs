use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{RemExecError, Result};

/// Default base directory for process state on the remote side.
pub const REMOTE_BASE: &str = "/tmp/rem-exec";

/// Per-process state directory layout.
pub struct ProcessDir {
    pub dir: PathBuf,
}

impl ProcessDir {
    pub fn new(base: &Path, id: &str) -> Self {
        Self {
            dir: base.join(id),
        }
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
    pub fn stdout_path(&self) -> PathBuf {
        self.dir.join("stdout")
    }
    pub fn stderr_path(&self) -> PathBuf {
        self.dir.join("stderr")
    }

    /// Read the status string from the status file.
    pub fn read_status(&self) -> Result<ProcessState> {
        let s = fs::read_to_string(self.status_path())
            .map_err(|_| RemExecError::ProcessNotFound {
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

    /// Read the runner PID.
    pub fn read_runner_pid(&self) -> Result<Option<u32>> {
        match fs::read_to_string(self.runner_pid_path()) {
            Ok(s) => Ok(s.trim().parse::<u32>().ok()),
            Err(_) => Ok(None),
        }
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
    Exited(i32),
    ExitedKilled,
    ExitedUnknown,
}

impl ProcessState {
    pub fn parse(s: &str) -> Self {
        match s {
            "running" => ProcessState::Running,
            "exited(killed)" => ProcessState::ExitedKilled,
            "exited(unknown)" => ProcessState::ExitedUnknown,
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
            ProcessState::ExitedKilled => write!(f, "exited(killed)"),
            ProcessState::ExitedUnknown => write!(f, "exited(unknown)"),
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
