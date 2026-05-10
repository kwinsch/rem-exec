use std::fs;
use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// Spawn a background thread that streams remote output to a local file.
///
/// Runs `ssh host rem-execd follow <id> <stream>` and pipes the raw bytes
/// into `local_path`. Retries with exponential backoff on SSH failure.
pub fn spawn_stream_thread(
    host: String,
    id: String,
    stream_name: String,
    local_path: std::path::PathBuf,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        stream_with_retry(&host, &id, &stream_name, &local_path);
    })
}

fn stream_with_retry(
    host: &str,
    id: &str,
    stream_name: &str,
    local_path: &std::path::Path,
) {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        match run_follow(host, id, stream_name, local_path) {
            Ok(()) => return, // follow completed normally (process exited)
            Err(e) => {
                eprintln!(
                    "stream {host}/{id}/{stream_name}: SSH error: {e}, retrying in {backoff:?}"
                );
                thread::sleep(backoff);
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

fn run_follow(
    host: &str,
    id: &str,
    stream_name: &str,
    local_path: &std::path::Path,
) -> io::Result<()> {
    // Ensure parent directory exists
    if let Some(parent) = local_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut child = Command::new("ssh")
        .arg(host)
        .arg(".local/bin/rem-execd")
        .arg("follow")
        .arg(id)
        .arg(stream_name)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let mut remote_stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no stdout from SSH"))?;

    // Append to local file (in case we're retrying after a disconnect)
    let mut local_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(local_path)?;

    let mut buf = [0u8; 8192];
    loop {
        match remote_stdout.read(&mut buf) {
            Ok(0) => break, // SSH/follow exited
            Ok(n) => {
                local_file.write_all(&buf[..n])?;
                local_file.flush()?;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }

    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            format!("SSH exited with {status}"),
        ))
    }
}
