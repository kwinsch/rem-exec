use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use crate::Encoding;

/// Wire protocol version. Bump when Request/Response shapes change incompatibly.
///
/// v2: request/response is framed JSON over `rxd serve` (stdin carries the
/// request line + optional raw body; stdout carries one response). This
/// replaces the v1 scheme of passing payloads as SSH argv, which let the
/// remote login shell re-parse command arguments and stdin data.
pub const PROTOCOL_VERSION: u32 = 2;

/// A request sent to `rxd serve` as a single JSON line on stdin.
///
/// For `Run`, `Start`, and `Write`, the bytes after the request line (the
/// "body") are the accompanying payload — process stdin for run/start, the
/// bytes to write for write. Because the whole request travels through the SSH
/// channel's stdin, no field is ever seen by the remote shell: command
/// arguments and input data are transported exactly, with no escaping.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Request {
    /// Run to completion, blocking up to `timeout_ms`. On timeout the process
    /// keeps running detached and a `Running` handle is returned instead.
    Run {
        command: Vec<String>,
        /// Working directory for the command (default: the SSH login dir).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// Environment overrides layered on the inherited environment.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
        #[serde(default)]
        timeout_ms: Option<u64>,
        /// Keep stdin open after the body is consumed (default: send EOF, so
        /// commands that read stdin don't block waiting for more input).
        #[serde(default)]
        keep_stdin_open: bool,
        /// When true (default), delete the process dir after a fully-inlined
        /// `completed` response so short agent runs do not accumulate remote
        /// state. Skipped when output is truncated (still readable via
        /// stdout/stderr) or the command is backgrounded (`running`).
        #[serde(default = "default_true")]
        ephemeral: bool,
    },
    /// Start a detached process and return its handle immediately.
    Start {
        command: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
    /// Block until a process exits or `timeout_ms` elapses.
    Wait {
        id: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    Status {
        id: String,
    },
    Read {
        id: String,
        stream: String,
        #[serde(default)]
        offset: Option<u64>,
        #[serde(default)]
        limit: Option<u64>,
    },
    /// Write the request body to the process's stdin.
    Write {
        id: String,
    },
    /// Write the request body to a file at `path`, atomically: stream to a temp
    /// file, apply mode/owner/group, then rename into place.
    Put {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<String>,
    },
    CloseStdin {
        id: String,
    },
    Kill {
        id: String,
    },
    List,
    Clean,
    Version,
}

/// Machine-branchable error categories. Agents switch on `code` instead of
/// substring-matching `message`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed request or unusable arguments.
    BadRequest,
    /// Process ID is not the 8-hex-digit form.
    InvalidProcessId,
    /// No such managed process.
    ProcessNotFound,
    /// Operation needs a running process but it has already exited.
    ProcessExited,
    /// A `run` exceeded its timeout (returned as `Running`, not an error) — or
    /// an internal wait timed out.
    Timeout,
    /// rxd is missing or too old on the remote; deploy resolves it.
    NotDeployed,
    /// Architecture or feature not supported.
    Unsupported,
    /// SSH could not reach the host (network/DNS/refused).
    SshUnreachable,
    /// SSH authentication failed.
    SshAuth,
    /// Unexpected internal failure.
    Internal,
}

impl ErrorCode {
    /// Whether retrying the same request could plausibly succeed.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            ErrorCode::NotDeployed | ErrorCode::SshUnreachable | ErrorCode::Internal
        )
    }
}

/// Response from rxd. `Follow` streams raw bytes and has no JSON envelope.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum Response {
    #[serde(rename = "started")]
    Started { id: String },

    /// A `run` that finished within its timeout.
    #[serde(rename = "completed")]
    Completed {
        id: String,
        /// Exit code, or `None` when terminated by a signal.
        exit_code: Option<i32>,
        /// Signal number when terminated by a signal, else `None`.
        signal: Option<i32>,
        duration_ms: u64,
        stdout: String,
        stdout_encoding: Encoding,
        stderr: String,
        stderr_encoding: Encoding,
        /// Total bytes on the remote (may exceed the inlined portion).
        stdout_size: u64,
        stderr_size: u64,
        /// True when the inlined stream is only the tail of a larger file.
        stdout_truncated: bool,
        stderr_truncated: bool,
    },

    /// A `run` that outlived its timeout and is now detached.
    #[serde(rename = "running")]
    Running {
        id: String,
        reason: String,
        stdout_size: u64,
        stderr_size: u64,
        hint: String,
    },

    #[serde(rename = "status")]
    Status {
        id: String,
        state: String,
        cmd: String,
        started: u64,
        ended: Option<u64>,
        /// Parsed exit code when the process exited normally.
        exit_code: Option<i32>,
        /// Parsed signal number when killed by a signal.
        signal: Option<i32>,
        stdout_size: u64,
        stderr_size: u64,
    },

    #[serde(rename = "output")]
    Output {
        /// Stream data, encoded per `encoding`.
        data: String,
        encoding: Encoding,
        /// Byte offset this chunk starts at.
        offset: u64,
        /// Total file size after this read.
        size: u64,
    },

    #[serde(rename = "written")]
    Written { bytes: usize },

    #[serde(rename = "copied")]
    Copied {
        path: String,
        bytes: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
    },

    #[serde(rename = "killed")]
    Killed { id: String },

    #[serde(rename = "list")]
    List { processes: Vec<ProcessSummary> },

    #[serde(rename = "cleaned")]
    Cleaned { removed: Vec<String> },

    #[serde(rename = "version")]
    Version { version: String, protocol: u32 },

    #[serde(rename = "error")]
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<ErrorCode>,
        #[serde(default, skip_serializing_if = "is_false")]
        retryable: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ProcessSummary {
    pub id: String,
    pub state: String,
    pub cmd: String,
}

/// Request from CLI to local daemon via Unix socket. The daemon forwards to the
/// remote over the same `serve` transport; payload bytes travel base64-encoded
/// so the socket stays plain JSON.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "action")]
pub enum DaemonRequest {
    #[serde(rename = "run")]
    Run {
        host: String,
        command: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        timeout_ms: Option<u64>,
        #[serde(default)]
        stdin_b64: Option<String>,
        #[serde(default)]
        keep_stdin_open: bool,
        /// See [`Request::Run::ephemeral`]. Default true (same as the wire).
        #[serde(default = "default_true")]
        ephemeral: bool,
    },
    #[serde(rename = "start")]
    Start {
        host: String,
        command: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    #[serde(rename = "wait")]
    Wait {
        host: String,
        id: String,
        timeout_ms: Option<u64>,
    },
    #[serde(rename = "status")]
    Status { host: String, id: String },
    #[serde(rename = "stdout")]
    Stdout {
        host: String,
        id: String,
        offset: Option<u64>,
        limit: Option<u64>,
    },
    #[serde(rename = "stderr")]
    Stderr {
        host: String,
        id: String,
        offset: Option<u64>,
        limit: Option<u64>,
    },
    #[serde(rename = "write")]
    Write {
        host: String,
        id: String,
        /// Exact bytes to write (newline handling already applied), base64.
        data_b64: String,
    },
    #[serde(rename = "close_stdin")]
    CloseStdin { host: String, id: String },
    #[serde(rename = "kill")]
    Kill { host: String, id: String },
    #[serde(rename = "list")]
    List { host: String },
    #[serde(rename = "clean")]
    Clean { host: String },
    #[serde(rename = "deploy")]
    Deploy { host: String },
    #[serde(rename = "daemon_status")]
    DaemonStatus,
    #[serde(rename = "daemon_stop")]
    DaemonStop,
}

/// Response from local daemon to CLI.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum DaemonResponse {
    #[serde(rename = "ok")]
    Ok { data: serde_json::Value },
    #[serde(rename = "error")]
    Error { message: String },
}

impl Response {
    /// Create an untyped error response.
    pub fn error(msg: impl Into<String>) -> Self {
        Response::Error {
            message: msg.into(),
            code: None,
            retryable: false,
            hint: None,
        }
    }

    /// Create a typed error response; `retryable` is derived from the code.
    pub fn error_code(code: ErrorCode, msg: impl Into<String>) -> Self {
        Response::Error {
            message: msg.into(),
            code: Some(code),
            retryable: code.retryable(),
            hint: None,
        }
    }

    /// Attach an actionable hint to an error response.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        if let Response::Error { hint: h, .. } = &mut self {
            *h = Some(hint.into());
        }
        self
    }
}
