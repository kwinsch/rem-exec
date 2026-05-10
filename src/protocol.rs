use serde::{Deserialize, Serialize};

/// Response from rem-execd for all actions (except `follow` which streams raw bytes).
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum Response {
    #[serde(rename = "started")]
    Started { id: String },

    #[serde(rename = "status")]
    Status {
        id: String,
        state: String,
        cmd: String,
        started: u64,
        ended: Option<u64>,
        stdout_size: u64,
        stderr_size: u64,
    },

    #[serde(rename = "output")]
    Output {
        /// Base64-encoded output data.
        data: String,
        /// Byte offset this chunk starts at.
        offset: u64,
        /// Total file size after this read.
        size: u64,
    },

    #[serde(rename = "written")]
    Written { bytes: usize },

    #[serde(rename = "killed")]
    Killed { id: String },

    #[serde(rename = "list")]
    List { processes: Vec<ProcessSummary> },

    #[serde(rename = "cleaned")]
    Cleaned { removed: Vec<String> },

    #[serde(rename = "error")]
    Error { message: String },
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ProcessSummary {
    pub id: String,
    pub state: String,
    pub cmd: String,
}

/// Request from CLI to local daemon via Unix socket.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "action")]
pub enum DaemonRequest {
    #[serde(rename = "start")]
    Start { host: String, command: Vec<String> },
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
        input: String,
        #[serde(default)]
        raw: bool,
    },
    #[serde(rename = "close_stdin")]
    CloseStdin { host: String, id: String },
    #[serde(rename = "kill")]
    Kill { host: String, id: String },
    #[serde(rename = "list")]
    List { host: String },
    #[serde(rename = "clean")]
    Clean { host: String },
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
    /// Convenience: create an error response.
    pub fn error(msg: impl Into<String>) -> Self {
        Response::Error {
            message: msg.into(),
        }
    }
}
