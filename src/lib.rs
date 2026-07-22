pub mod daemon;
pub mod deploy;
pub mod error;
pub mod process;
pub mod protocol;
pub mod remote;
pub mod ssh;

/// Base64-encode a byte slice (standard alphabet, padded).
pub fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[(triple >> 18 & 0x3F) as usize] as char);
        result.push(CHARS[(triple >> 12 & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[(triple >> 6 & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

/// Decode a standard-alphabet base64 string. ASCII whitespace is ignored so
/// wrapped input decodes cleanly; any other non-alphabet byte is an error.
pub fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let mut quad = [0u8; 4];
    let mut n = 0usize;
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut padding = 0usize;

    for &b in input.as_bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            padding += 1;
            quad[n] = 0;
            n += 1;
        } else {
            if padding != 0 {
                return Err("base64: data after padding".to_string());
            }
            quad[n] = val(b).ok_or_else(|| format!("base64: invalid byte {b:#x}"))?;
            n += 1;
        }
        if n == 4 {
            out.push((quad[0] << 2) | (quad[1] >> 4));
            if padding < 2 {
                out.push((quad[1] << 4) | (quad[2] >> 2));
            }
            if padding < 1 {
                out.push((quad[2] << 6) | quad[3]);
            }
            n = 0;
            if padding != 0 {
                break;
            }
        }
    }

    if n != 0 {
        return Err("base64: truncated input (length not a multiple of 4)".to_string());
    }
    Ok(out)
}

/// How the bytes of a text field are carried in JSON.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    /// Valid UTF-8 with no NUL — carried verbatim in the JSON string.
    Utf8,
    /// Arbitrary bytes — base64-encoded in the JSON string.
    Base64,
}

/// Render bytes for JSON transport: verbatim UTF-8 when it is clean text,
/// otherwise base64. Lets agents read text output directly without decoding,
/// while staying binary-safe.
pub fn encode_bytes(data: &[u8]) -> (String, Encoding) {
    match std::str::from_utf8(data) {
        Ok(s) if !s.contains('\0') => (s.to_string(), Encoding::Utf8),
        _ => (base64_encode(data), Encoding::Base64),
    }
}

#[cfg(test)]
mod tests {
    use super::{Encoding, base64_decode, base64_encode, encode_bytes};

    #[test]
    fn base64_encode_handles_empty_and_padding_cases() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"hello\n"), "aGVsbG8K");
    }

    #[test]
    fn base64_decode_reverses_encode_for_all_padding_cases() {
        for sample in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"hello\n",
            &[0u8, 1, 2, 253, 254, 255],
        ] {
            assert_eq!(base64_decode(&base64_encode(sample)).unwrap(), sample);
        }
    }

    #[test]
    fn base64_decode_ignores_whitespace_and_rejects_garbage() {
        assert_eq!(base64_decode("Zm9v\n").unwrap(), b"foo");
        assert_eq!(base64_decode(" Zm 9v ").unwrap(), b"foo");
        assert!(base64_decode("Zm9v!").is_err());
        assert!(base64_decode("Zg=").is_err()); // truncated
    }

    #[test]
    fn encode_bytes_picks_utf8_for_text_and_base64_for_binary() {
        assert_eq!(
            encode_bytes(b"plain text\n"),
            ("plain text\n".to_string(), Encoding::Utf8)
        );
        // Embedded NUL and invalid UTF-8 fall back to base64.
        assert_eq!(encode_bytes(b"a\0b").1, Encoding::Base64);
        assert_eq!(encode_bytes(&[0xff, 0xfe]).1, Encoding::Base64);
    }
}
