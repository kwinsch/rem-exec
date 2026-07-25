//! Length-prefixed framing for request bodies of unknown length.
//!
//! A sized transfer (`put LOCAL`) declares its byte count up front and the
//! receiver simply compares. A stdin transfer (`put -`) has no length known in
//! advance, so completeness has to travel *inside* the stream: the sender emits
//! length-prefixed chunks and a terminator; anything that ends without the
//! terminator is a truncated transfer, not a short file.
//!
//! ```text
//! frame      := u32_le len (len > 0) | len bytes
//! terminator := u32_le 0            | u64_le total_bytes
//! ```
//!
//! The trailing total is redundant with the accumulated count and is checked
//! anyway: it turns a mis-framed stream into a typed rejection instead of a
//! plausible-looking file.

use std::io::{self, Read, Write};

/// Payload bytes per frame. Matches the pipe buffer size we stream through.
pub const CHUNK_SIZE: usize = 64 * 1024;

/// Largest frame the reader will accept. The writer never exceeds
/// [`CHUNK_SIZE`]; a larger length means a corrupt or mis-framed stream, and
/// failing here beats reading garbage until EOF.
const MAX_FRAME: u32 = 8 * 1024 * 1024;

/// Stream `src` to `dst` as length-prefixed frames, ending with the terminator.
/// Returns the payload byte count (excluding framing overhead).
pub fn write_framed<R: Read + ?Sized, W: Write + ?Sized>(
    src: &mut R,
    dst: &mut W,
) -> io::Result<u64> {
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut total: u64 = 0;

    loop {
        let n = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        // n <= CHUNK_SIZE, so the cast cannot truncate.
        dst.write_all(&(n as u32).to_le_bytes())?;
        dst.write_all(&buf[..n])?;
        total += n as u64;
    }

    dst.write_all(&0u32.to_le_bytes())?;
    dst.write_all(&total.to_le_bytes())?;
    dst.flush()?;
    Ok(total)
}

/// Reads the payload out of a framed stream, tracking whether the terminator
/// arrived. Implements [`Read`], so the receiver keeps using `io::copy` and
/// stays constant-memory.
///
/// Returning `Ok(0)` means "the sender said it was done" — check
/// [`FramedReader::is_complete`] before installing anything, because a stream
/// cut mid-transfer surfaces as [`io::ErrorKind::UnexpectedEof`] instead.
pub struct FramedReader<R: Read> {
    inner: R,
    /// Payload bytes left in the frame currently being read.
    remaining: usize,
    /// Payload bytes seen so far.
    total: u64,
    complete: bool,
}

impl<R: Read> FramedReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            remaining: 0,
            total: 0,
            complete: false,
        }
    }

    /// True once the terminator arrived and its declared total matched what was
    /// actually read. False means the stream ended early — reject the transfer.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Payload bytes read so far.
    pub fn total(&self) -> u64 {
        self.total
    }

    fn read_u32(&mut self) -> io::Result<u32> {
        let mut b = [0u8; 4];
        self.inner.read_exact(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    fn read_u64(&mut self) -> io::Result<u64> {
        let mut b = [0u8; 8];
        self.inner.read_exact(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }
}

impl<R: Read> Read for FramedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.complete || buf.is_empty() {
            return Ok(0);
        }

        if self.remaining == 0 {
            let len = self.read_u32()?;
            if len == 0 {
                let declared = self.read_u64()?;
                if declared != self.total {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "framing mismatch: trailer declares {declared} bytes, read {}",
                            self.total
                        ),
                    ));
                }
                self.complete = true;
                return Ok(0);
            }
            if len > MAX_FRAME {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("frame length {len} exceeds {MAX_FRAME}"),
                ));
            }
            self.remaining = len as usize;
        }

        let want = buf.len().min(self.remaining);
        // A frame header promised these bytes, so EOF inside one is truncation.
        self.inner.read_exact(&mut buf[..want])?;
        self.remaining -= want;
        self.total += want as u64;
        Ok(want)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(payload: &[u8]) -> (Vec<u8>, bool, u64) {
        let mut wire = Vec::new();
        write_framed(&mut &payload[..], &mut wire).unwrap();
        let mut reader = FramedReader::new(&wire[..]);
        let mut out = Vec::new();
        io::copy(&mut reader, &mut out).unwrap();
        (out, reader.is_complete(), reader.total())
    }

    #[test]
    fn roundtrips_payloads_across_frame_boundaries() {
        for size in [0usize, 1, 100, CHUNK_SIZE - 1, CHUNK_SIZE, CHUNK_SIZE + 1] {
            let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let (out, complete, total) = roundtrip(&payload);
            assert_eq!(out, payload, "payload of {size} bytes");
            assert!(complete, "payload of {size} bytes must be complete");
            assert_eq!(total, size as u64);
        }
    }

    #[test]
    fn truncated_stream_is_unexpected_eof_not_a_short_read() {
        let payload = vec![7u8; 5000];
        let mut wire = Vec::new();
        write_framed(&mut &payload[..], &mut wire).unwrap();

        // Cut anywhere before the terminator: never a clean end.
        for cut in [1, 3, 4, 5, 2000, wire.len() - 1] {
            let mut reader = FramedReader::new(&wire[..cut]);
            let mut out = Vec::new();
            let err = io::copy(&mut reader, &mut out).expect_err("truncation must error");
            assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "cut at {cut}");
            assert!(!reader.is_complete(), "cut at {cut}");
        }
    }

    #[test]
    fn payload_without_terminator_never_reports_complete() {
        // Exactly one full frame, then the stream dies — the shape a killed
        // sender produces. The bytes are all there; the promise is not.
        let mut wire = Vec::new();
        wire.extend_from_slice(&4u32.to_le_bytes());
        wire.extend_from_slice(b"data");

        let mut reader = FramedReader::new(&wire[..]);
        let mut out = Vec::new();
        let err = io::copy(&mut reader, &mut out).expect_err("missing terminator must error");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert!(!reader.is_complete());
    }

    #[test]
    fn trailer_that_disagrees_with_the_payload_is_rejected() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&4u32.to_le_bytes());
        wire.extend_from_slice(b"data");
        wire.extend_from_slice(&0u32.to_le_bytes());
        wire.extend_from_slice(&99u64.to_le_bytes()); // lies

        let mut reader = FramedReader::new(&wire[..]);
        let mut out = Vec::new();
        let err = io::copy(&mut reader, &mut out).expect_err("bad trailer must error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(!reader.is_complete());
    }

    #[test]
    fn empty_payload_is_complete_but_zero_length() {
        let (out, complete, total) = roundtrip(b"");
        assert!(out.is_empty());
        assert!(complete, "an empty stream is well-formed…");
        assert_eq!(total, 0, "…and the caller decides whether 0 bytes is legal");
    }
}
