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

    /// Get the local cache directory for a host.
    pub fn host_dir(&self, host: &str) -> PathBuf {
        self.local_base.join(safe_path_component(host))
    }

    /// Get the local directory for a host+process.
    pub fn local_dir(&self, host: &str, id: &str) -> PathBuf {
        self.host_dir(host).join(safe_path_component(id))
    }

    /// Summary of tracked processes across all hosts.
    pub fn summary(&self) -> (usize, usize) {
        let hosts = self.hosts.len();
        let procs: usize = self.hosts.values().map(|h| h.processes.len()).sum();
        (hosts, procs)
    }
}

fn safe_path_component(input: &str) -> String {
    use std::fmt::Write;

    let mut out = String::with_capacity(input.len() + 2);
    out.push_str("c-");

    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => out.push(b as char),
            _ => write!(&mut out, "%{b:02x}").expect("writing to string cannot fail"),
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_cache_paths_do_not_use_raw_host_or_process_components() {
        let state = DaemonState::new(PathBuf::from("/cache"));

        assert_eq!(
            state.local_dir("example.com", "0123abcd"),
            PathBuf::from("/cache/c-example.com/c-0123abcd")
        );
        assert_eq!(
            state.local_dir("../host", "../process"),
            PathBuf::from("/cache/c-..%2fhost/c-..%2fprocess")
        );
    }
}
