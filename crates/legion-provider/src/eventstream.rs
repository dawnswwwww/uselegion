//! AWS event-stream binary frame decoding (used by Bedrock ConverseStream).
//!
//! Frame layout: `[total_len:u32 BE][headers_len:u32 BE][prelude_crc:u32 BE]
//! [headers][payload][message_crc:u32 BE]`. The prelude CRC covers the first
//! 8 bytes; the message CRC covers everything before it. Both use the
//! reflected IEEE CRC32, implemented here by hand to avoid a dependency.

use crate::types::ProviderError;
use std::collections::HashMap;

const PRELUDE_LEN: usize = 12;
const TRAILER_LEN: usize = 4;

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC32_TABLE: [u32; 256] = build_crc32_table();

/// Reflected IEEE CRC32 (same polynomial as zlib / PNG / AWS event streams).
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF;
    for &b in data {
        crc = CRC32_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

fn be_u32(buf: &[u8]) -> u32 {
    u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]])
}

/// Decode one frame from the front of `buf`.
///
/// Returns `Ok(None)` when `buf` does not yet hold a complete frame,
/// `Ok(Some((event_type, payload, consumed)))` for a decoded frame, and an
/// error when a complete frame fails CRC validation or is structurally
/// invalid. The `:event-type` header value is returned; frames without one
/// yield an empty string.
pub(crate) fn decode_frame(buf: &[u8]) -> Result<Option<(String, Vec<u8>, usize)>, ProviderError> {
    if buf.len() < PRELUDE_LEN {
        return Ok(None);
    }
    let total_len = be_u32(&buf[0..4]) as usize;
    let headers_len = be_u32(&buf[4..8]) as usize;
    if total_len < PRELUDE_LEN + TRAILER_LEN || headers_len > total_len - PRELUDE_LEN - TRAILER_LEN
    {
        return Err(ProviderError::StreamAborted(
            "event stream frame length invalid".to_string(),
        ));
    }
    if crc32(&buf[..8]) != be_u32(&buf[8..12]) {
        return Err(ProviderError::StreamAborted(
            "event stream crc mismatch".to_string(),
        ));
    }
    if buf.len() < total_len {
        return Ok(None);
    }
    if crc32(&buf[..total_len - TRAILER_LEN]) != be_u32(&buf[total_len - TRAILER_LEN..total_len]) {
        return Err(ProviderError::StreamAborted(
            "event stream crc mismatch".to_string(),
        ));
    }

    let headers = parse_headers(&buf[PRELUDE_LEN..PRELUDE_LEN + headers_len])?;
    let payload = buf[PRELUDE_LEN + headers_len..total_len - TRAILER_LEN].to_vec();
    let event_type = headers.get(":event-type").cloned().unwrap_or_default();
    Ok(Some((event_type, payload, total_len)))
}

/// Streaming wrapper around [`decode_frame`]: push bytes as they arrive and
/// drain complete frames.
#[derive(Debug, Default)]
pub(crate) struct EventStreamDecoder {
    buf: Vec<u8>,
}

impl EventStreamDecoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub(crate) fn next_frame(&mut self) -> Result<Option<(String, Vec<u8>)>, ProviderError> {
        match decode_frame(&self.buf)? {
            Some((event_type, payload, consumed)) => {
                self.buf.drain(..consumed);
                Ok(Some((event_type, payload)))
            }
            None => Ok(None),
        }
    }
}

/// Parse the header block of a frame. Only string values (type 7, plus byte
/// strings of type 6) carry meaning for us; scalar types are consumed so the
/// cursor stays aligned and rendered as their display value.
fn parse_headers(mut buf: &[u8]) -> Result<HashMap<String, String>, ProviderError> {
    let mut headers = HashMap::new();
    while !buf.is_empty() {
        let name_len = take(&mut buf, 1)?[0] as usize;
        let name = String::from_utf8_lossy(take(&mut buf, name_len)?).into_owned();
        let value_type = take(&mut buf, 1)?[0];
        let value = match value_type {
            0 => "true".to_string(),
            1 => "false".to_string(),
            2 => take(&mut buf, 1)?[0].to_string(),
            3 => {
                let b = take(&mut buf, 2)?;
                i16::from_be_bytes([b[0], b[1]]).to_string()
            }
            4 => i32::from_be_bytes(be4(take(&mut buf, 4)?)).to_string(),
            5 | 8 => i64::from_be_bytes(be8(take(&mut buf, 8)?)).to_string(),
            6 | 7 => {
                let len_bytes = take(&mut buf, 2)?;
                let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
                String::from_utf8_lossy(take(&mut buf, len)?).into_owned()
            }
            9 => {
                take(&mut buf, 16)?;
                String::new()
            }
            other => {
                return Err(ProviderError::StreamAborted(format!(
                    "unknown event stream header type {other}"
                )));
            }
        };
        headers.insert(name, value);
    }
    Ok(headers)
}

fn be4(buf: &[u8]) -> [u8; 4] {
    [buf[0], buf[1], buf[2], buf[3]]
}

fn be8(buf: &[u8]) -> [u8; 8] {
    [
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ]
}

fn take<'a>(buf: &mut &'a [u8], n: usize) -> Result<&'a [u8], ProviderError> {
    if buf.len() < n {
        return Err(ProviderError::StreamAborted(
            "event stream header truncated".to_string(),
        ));
    }
    let (head, tail) = buf.split_at(n);
    *buf = tail;
    Ok(head)
}

/// Build a well-formed event-stream frame for tests.
#[cfg(test)]
pub(crate) fn encode_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut header_bytes = Vec::new();
    for (name, value) in headers {
        header_bytes.push(name.len() as u8);
        header_bytes.extend_from_slice(name.as_bytes());
        header_bytes.push(7); // string
        header_bytes.extend_from_slice(&(value.len() as u16).to_be_bytes());
        header_bytes.extend_from_slice(value.as_bytes());
    }
    let total_len = (PRELUDE_LEN + header_bytes.len() + payload.len() + TRAILER_LEN) as u32;
    let mut frame = Vec::with_capacity(total_len as usize);
    frame.extend_from_slice(&total_len.to_be_bytes());
    frame.extend_from_slice(&(header_bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(&crc32(&frame).to_be_bytes());
    frame.extend_from_slice(&header_bytes);
    frame.extend_from_slice(payload);
    frame.extend_from_slice(&crc32(&frame).to_be_bytes());
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn decodes_roundtrip_frame() {
        let payload = br#"{"contentBlockIndex":0}"#;
        let frame = encode_frame(
            &[
                (":event-type", "contentBlockDelta"),
                (":message-type", "event"),
                (":content-type", "application/json"),
            ],
            payload,
        );
        let (event_type, decoded, consumed) = decode_frame(&frame).unwrap().unwrap();
        assert_eq!(event_type, "contentBlockDelta");
        assert_eq!(decoded, payload);
        assert_eq!(consumed, frame.len());
    }

    #[test]
    fn rejects_corrupt_prelude_crc() {
        let mut frame = encode_frame(&[(":event-type", "x")], b"data");
        // Flip a byte inside the prelude without fixing the prelude CRC.
        frame[1] ^= 0xFF;
        match decode_frame(&frame) {
            Err(ProviderError::StreamAborted(msg)) => {
                assert!(msg.contains("crc mismatch"), "msg = {msg}")
            }
            Err(err) => panic!("expected StreamAborted, got {err}"),
            Ok(_) => panic!("corrupt prelude must fail"),
        }
    }

    #[test]
    fn rejects_corrupt_message_crc() {
        let mut frame = encode_frame(&[(":event-type", "x")], b"data");
        // Flip a payload byte: prelude stays valid, message CRC does not.
        let last = frame.len() - 6;
        frame[last] ^= 0xFF;
        match decode_frame(&frame) {
            Err(ProviderError::StreamAborted(msg)) => {
                assert!(msg.contains("crc mismatch"), "msg = {msg}")
            }
            Err(err) => panic!("expected StreamAborted, got {err}"),
            Ok(_) => panic!("corrupt payload must fail"),
        }
    }

    #[test]
    fn incomplete_frame_returns_none_then_completes() {
        let frame = encode_frame(
            &[(":event-type", "messageStop")],
            br#"{"stopReason":"end_turn"}"#,
        );
        let mut decoder = EventStreamDecoder::new();
        // Feed everything except the final CRC byte.
        decoder.push(&frame[..frame.len() - 1]);
        assert!(decoder.next_frame().unwrap().is_none());
        decoder.push(&frame[frame.len() - 1..]);
        let (event_type, payload) = decoder.next_frame().unwrap().unwrap();
        assert_eq!(event_type, "messageStop");
        assert_eq!(payload, br#"{"stopReason":"end_turn"}"#.to_vec());
        assert!(decoder.next_frame().unwrap().is_none());
    }

    #[test]
    fn decodes_consecutive_frames() {
        let first = encode_frame(&[(":event-type", "a")], b"1");
        let second = encode_frame(&[(":event-type", "b")], b"22");
        let mut decoder = EventStreamDecoder::new();
        let mut bytes = first.clone();
        bytes.extend_from_slice(&second);
        decoder.push(&bytes);

        let (event_type, payload) = decoder.next_frame().unwrap().unwrap();
        assert_eq!(event_type, "a");
        assert_eq!(payload, b"1".to_vec());
        let (event_type, payload) = decoder.next_frame().unwrap().unwrap();
        assert_eq!(event_type, "b");
        assert_eq!(payload, b"22".to_vec());
        assert!(decoder.next_frame().unwrap().is_none());
    }

    #[test]
    fn parses_headers_into_map() {
        let frame = encode_frame(
            &[
                (":event-type", "metadata"),
                (":message-type", "event"),
                (":content-type", "application/json"),
            ],
            b"{}",
        );
        // Re-parse the header block directly to exercise parse_headers.
        let headers_len = be_u32(&frame[4..8]) as usize;
        let headers = parse_headers(&frame[PRELUDE_LEN..PRELUDE_LEN + headers_len]).unwrap();
        assert_eq!(headers.get(":event-type").unwrap(), "metadata");
        assert_eq!(headers.get(":message-type").unwrap(), "event");
        assert_eq!(headers.get(":content-type").unwrap(), "application/json");
    }

    #[test]
    fn rejects_truncated_header_block() {
        let mut frame = encode_frame(&[(":event-type", "x")], b"");
        // Corrupt a name-length byte inside the header block, then fix the
        // message CRC so the failure is in header parsing, not CRC checks.
        let name_len_pos = PRELUDE_LEN;
        frame[name_len_pos] = 200;
        let crc = crc32(&frame[..frame.len() - TRAILER_LEN]);
        let trailer = frame.len() - TRAILER_LEN;
        frame[trailer..].copy_from_slice(&crc.to_be_bytes());
        match decode_frame(&frame) {
            Err(ProviderError::StreamAborted(_)) => {}
            Err(err) => panic!("expected StreamAborted, got {err}"),
            Ok(_) => panic!("truncated header must fail"),
        }
    }
}
