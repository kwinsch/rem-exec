use std::collections::HashMap;
use std::path::PathBuf;
use std::thread::JoinHandle;

/// In-memory state of the daemon.
pub struct DaemonState {
    pub hosts: HashMap<String, HostState>,
    pub local_base: PathBuf,
}

/// Per-host state tracking.
pub struct HostState {
    pub processes: HashMap<String, TrackedProcess>,
}

/// A process being tracked by the daemon with its streaming threads.
pub struct TrackedProcess {
    pub id: String,
    pub local_dir: PathBuf,
    pub stdout_thread: Option<JoinHandle<()>>,
    pub stderr_thread: Option<JoinHandle<()>>,
}

impl DaemonState {
    pub fn new(local_base: PathBuf) -> Self {
        Self {
            hosts: HashMap::new(),
            local_base,
        }
    }

    /// Get or create the HostState for a given host.
    pub fn host_mut(&mut self, host: &str) -> &mut HostState {
        self.hosts
            .entry(host.to_string())
            .or_insert_with(|| HostState {
                processes: HashMap::new(),
            })
    }

    /// Get the local directory for a host+process.
    pub fn local_dir(&self, host: &str, id: &str) -> PathBuf {
        self.local_base.join(host).join(id)
    }

    /// Summary of tracked processes across all hosts.
    pub fn summary(&self) -> (usize, usize) {
        let hosts = self.hosts.len();
        let procs: usize = self.hosts.values().map(|h| h.processes.len()).sum();
        (hosts, procs)
    }
}
