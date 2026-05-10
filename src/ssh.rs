use std::process::{Command, Output};

use crate::error::{RemExecError, Result};
use crate::protocol::Response;

/// Execute a rem-execd command on a remote host via SSH.
pub fn ssh_exec(host: &str, args: &[&str]) -> Result<Response> {
    let output = ssh_raw(host, args)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RemExecError::Ssh(format!(
            "SSH exited with {}: {stderr}",
            output.status
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stdout = stdout.trim();
    if stdout.is_empty() {
        return Err(RemExecError::Ssh("empty response from remote".to_string()));
    }

    serde_json::from_str(stdout).map_err(|e| {
        RemExecError::Protocol(format!("invalid JSON from remote: {e}: {stdout}"))
    })
}

/// The remote binary name. Uses ~/.local/bin path since it may not be in
/// the non-login SSH PATH.
const REMOTE_BIN: &str = ".local/bin/rem-execd";

/// Execute a raw SSH command, returning the Output.
pub fn ssh_raw(host: &str, args: &[&str]) -> Result<Output> {
    let mut cmd = Command::new("ssh");
    cmd.arg(host);
    cmd.arg(REMOTE_BIN);
    cmd.args(args);
    cmd.output().map_err(RemExecError::Io)
}

/// Build rem-execd arguments for a given action.
/// Returns the args as owned Strings for lifetime convenience.
pub struct RemoteArgs {
    args: Vec<String>,
}

impl RemoteArgs {
    pub fn start(command: &[String]) -> Self {
        let mut args = vec!["start".to_string(), "--".to_string()];
        args.extend(command.iter().cloned());
        Self { args }
    }

    pub fn status(id: &str) -> Self {
        Self {
            args: vec!["status".to_string(), id.to_string()],
        }
    }

    pub fn read(id: &str, stream: &str, offset: Option<u64>) -> Self {
        let mut args = vec!["read".to_string(), id.to_string(), stream.to_string()];
        if let Some(off) = offset {
            args.push("--offset".to_string());
            args.push(off.to_string());
        }
        Self { args }
    }

    pub fn write(id: &str, input: &str) -> Self {
        Self {
            args: vec!["write".to_string(), id.to_string(), input.to_string()],
        }
    }

    pub fn kill(id: &str) -> Self {
        Self {
            args: vec!["kill".to_string(), id.to_string()],
        }
    }

    pub fn list() -> Self {
        Self {
            args: vec!["list".to_string()],
        }
    }

    pub fn clean() -> Self {
        Self {
            args: vec!["clean".to_string()],
        }
    }

    pub fn as_str_slice(&self) -> Vec<&str> {
        self.args.iter().map(|s| s.as_str()).collect()
    }
}
