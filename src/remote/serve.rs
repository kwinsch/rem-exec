use std::io::{BufRead, BufReader, Read, Write};
use std::process::ExitCode;

use crate::process::{ensure_base_dir, remote_base};
use crate::protocol::{ErrorCode, PROTOCOL_VERSION, Request, Response};
use crate::remote::{actions, hostinfo, start};

/// Handle one framed request from stdin and write one JSON response to stdout.
///
/// Wire framing: the request is a single JSON line; any bytes after the newline
/// are the request body (process stdin for run/start, bytes to write for
/// write). Because the entire request travels through the SSH channel's stdin,
/// the remote login shell never parses a single field — command arguments and
/// input data are transported exactly, with no escaping.
pub fn serve() -> ExitCode {
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());

    let mut line = Vec::new();
    if let Err(e) = read_request_line(&mut reader, &mut line) {
        return emit(Response::error_code(
            ErrorCode::BadRequest,
            format!("failed to read request: {e}"),
        ));
    }
    if line.is_empty() {
        return emit(Response::error_code(ErrorCode::BadRequest, "empty request"));
    }

    let request: Request = match serde_json::from_slice(&line) {
        Ok(r) => r,
        Err(e) => {
            return emit(Response::error_code(
                ErrorCode::BadRequest,
                format!("invalid request JSON: {e}"),
            ));
        }
    };

    // Version and Ping must answer on a fresh host, before any state dir exists.
    if matches!(request, Request::Version) {
        return emit(Response::Version {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: PROTOCOL_VERSION,
        });
    }
    if matches!(request, Request::Ping) {
        let id = hostinfo::identity();
        return emit(Response::Ping {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: PROTOCOL_VERSION,
            arch: id.arch,
            os: id.os,
            kernel: id.kernel,
            hostname: id.hostname,
            distro_id: id.distro_id,
            distro_version: id.distro_version,
        });
    }
    // Get streams raw file bytes on stdout (after a JSON header), so it can't go
    // through emit(); it also needs no state dir. Handle it here like version.
    if let Request::Get { path } = &request {
        return serve_get(path);
    }

    let base = remote_base();
    if let Err(e) = ensure_base_dir(&base) {
        return emit(Response::error_code(ErrorCode::Internal, e.to_string()));
    }

    let response = match request {
        Request::Run {
            command,
            cwd,
            env,
            timeout_ms,
            keep_stdin_open,
            ephemeral,
        } => {
            let body = read_to_end(&mut reader);
            actions::run(
                &command,
                cwd.as_deref(),
                &env,
                timeout_ms,
                &body,
                keep_stdin_open,
                ephemeral,
            )
        }
        Request::Start { command, cwd, env } => {
            // Start does not consume a body here; large stdin uses the dedicated
            // pipe-stdin channel. Drain so the writer never sees a broken pipe.
            drain(&mut reader);
            match start::start(&command, cwd.as_deref(), &env) {
                Ok(r) => r,
                Err(e) => Response::error_code(ErrorCode::Internal, e.to_string()),
            }
        }
        Request::Wait { id, timeout_ms } => actions::wait(&id, timeout_ms),
        Request::Write { id } => {
            let body = read_to_end(&mut reader);
            actions::write_stdin(&id, &body)
        }
        Request::Put {
            path,
            size,
            mode,
            owner,
            group,
        } => actions::put(&mut reader, &path, size, mode, owner.as_deref(), group.as_deref()),
        Request::Status { id } => actions::status(&id),
        Request::Read {
            id,
            stream,
            offset,
            limit,
        } => actions::read_output(&id, &stream, offset, limit),
        Request::CloseStdin { id } => actions::close_stdin(&id),
        Request::Kill { id } => actions::kill(&id),
        Request::List => actions::list(),
        Request::Clean => actions::clean(),
        Request::Version | Request::Ping | Request::Get { .. } => {
            unreachable!("handled above")
        }
    };

    emit(response)
}

/// Read the JSON request line (up to the first newline), stripping the newline.
/// Bytes buffered past the newline stay available for the body read.
fn read_request_line<R: BufRead>(reader: &mut R, out: &mut Vec<u8>) -> std::io::Result<()> {
    reader.read_until(b'\n', out)?;
    if out.last() == Some(&b'\n') {
        out.pop();
    }
    Ok(())
}

fn read_to_end<R: Read>(reader: &mut R) -> Vec<u8> {
    let mut body = Vec::new();
    let _ = reader.read_to_end(&mut body);
    body
}

fn drain<R: Read>(reader: &mut R) {
    let mut sink = Vec::new();
    let _ = reader.read_to_end(&mut sink);
}

fn emit(response: Response) -> ExitCode {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    write_line(&mut lock, response)
}

/// Write one JSON response line to `out`. Transport success is exit 0; the
/// success/failure of the operation is carried in the JSON itself.
fn write_line<W: Write>(out: &mut W, response: Response) -> ExitCode {
    let json = serde_json::to_string(&response).unwrap_or_else(|e| {
        format!("{{\"type\":\"error\",\"message\":\"serialize failed: {e}\"}}")
    });
    let _ = out.write_all(json.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
    ExitCode::SUCCESS
}

/// Stream a remote file to stdout: a `GetStream` header line (size + mode), then
/// exactly `size` raw bytes. On open/stat failure, a single `Error` line and no
/// body. A read error mid-stream simply ends the body short — the client's
/// received-vs-declared check rejects the partial file.
fn serve_get(path: &str) -> ExitCode {
    use std::os::unix::fs::PermissionsExt;

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return write_line(&mut out, get_open_error(path, &e)),
    };
    let meta = match f.metadata() {
        Ok(m) if m.is_file() => m,
        Ok(_) => {
            return write_line(
                &mut out,
                Response::error_code(ErrorCode::BadRequest, format!("not a regular file: {path}")),
            );
        }
        Err(e) => {
            return write_line(
                &mut out,
                Response::error_code(ErrorCode::Internal, format!("stat {path}: {e}")),
            );
        }
    };

    let header = Response::GetStream {
        size: meta.len(),
        mode: meta.permissions().mode() & 0o777,
    };
    let json = serde_json::to_string(&header).unwrap_or_default();
    if out.write_all(json.as_bytes()).is_err() || out.write_all(b"\n").is_err() {
        return ExitCode::FAILURE;
    }
    match std::io::copy(&mut f, &mut out) {
        Ok(_) => {
            let _ = out.flush();
            ExitCode::SUCCESS
        }
        Err(_) => ExitCode::FAILURE,
    }
}

/// Map a file-open error to a typed response for the `get` header.
fn get_open_error(path: &str, e: &std::io::Error) -> Response {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound => {
            Response::error_code(ErrorCode::NotFound, format!("no such file: {path}"))
        }
        ErrorKind::PermissionDenied => Response::error_code(
            ErrorCode::BadRequest,
            format!("cannot read {path}: permission denied"),
        ),
        _ => Response::error_code(ErrorCode::Internal, format!("open {path}: {e}")),
    }
}
