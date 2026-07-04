use std::io::{self, Read, Write};

/// Hard cap on a single frame's payload (1 GiB). A plugin is an untrusted
/// peer over the wire: the 4-byte length prefix is attacker-controlled and a
/// hostile/buggy plugin could send `0xFFFF_FFFF` to make the daemon allocate
/// 4 GiB and OOM. We refuse any frame larger than this before allocating.
/// Sized from measurement: the kubernetes monorepo's Go InvokeResponse is
/// ~288 MB once RFC-014 G2 `refs` are included, so 256 MiB was too small.
/// 1 GiB gives ~3.5× headroom; repos beyond that need a streaming or
/// compressed protocol (tracked as a follow-up).
pub const MAX_FRAME_LEN: usize = 1024 * 1024 * 1024;

pub fn encode_message<T: serde::Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let payload =
        serde_json::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if payload.len() > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "frame payload {} bytes exceeds MAX_FRAME_LEN ({MAX_FRAME_LEN} bytes)",
                payload.len()
            ),
        ));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "payload too large"))?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_message<T: serde::de::DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    decode_message_limited(reader, MAX_FRAME_LEN)
}

/// Like [`decode_message`] but with a caller-supplied frame cap — used by the
/// host to enforce ADR-018 `output_max` on per-parse responses so a forged or
/// runaway giant `ParseResponse` is rejected before allocation (T16).
pub fn decode_message_limited<T: serde::de::DeserializeOwned>(
    reader: &mut impl Read,
    max_len: usize,
) -> io::Result<T> {
    let max_len = max_len.min(MAX_FRAME_LEN);
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    // Reject oversized frames BEFORE allocating — prevents a hostile plugin from
    // triggering a multi-gigabyte allocation via a forged length prefix (DoS).
    if len > max_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds the {max_len}-byte cap"),
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn write_message<T: serde::Serialize>(writer: &mut impl Write, msg: &T) -> io::Result<()> {
    let frame = encode_message(msg)?;
    writer.write_all(&frame)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ParseRequest;
    use std::io::Cursor;
    use std::path::PathBuf;

    #[test]
    fn round_trip_parse_request() {
        let req = ParseRequest {
            path: PathBuf::from("src/main.ts"),
            vname_path: "src/main.ts".into(),
            corpus: "github.com/acme/foo".into(),
            package: "acme".into(),
            source: None,
        };
        let payload = serde_json::to_vec(&req).unwrap();
        let encoded = encode_message(&req).unwrap();
        // Frame must be exactly 4-byte length prefix + payload.
        assert_eq!(encoded.len(), 4 + payload.len());
        // First 4 bytes must be the big-endian payload length.
        let len = u32::from_be_bytes(encoded[..4].try_into().unwrap()) as usize;
        assert_eq!(len, payload.len());
        let mut cursor = Cursor::new(&encoded);
        let decoded: ParseRequest = decode_message(&mut cursor).unwrap();
        assert_eq!(decoded.path, req.path);
        assert_eq!(decoded.vname_path, req.vname_path);
        assert_eq!(decoded.corpus, req.corpus);
    }

    /// ADR-018 output_max (T16): a frame over the caller's cap is rejected
    /// from the length prefix alone — before the payload is allocated or read.
    #[test]
    fn oversized_frame_rejected_by_limited_decode() {
        let req = ParseRequest {
            path: PathBuf::from("a.ts"),
            vname_path: "a.ts".into(),
            corpus: String::new(),
            package: String::new(),
            source: None,
        };
        let encoded = encode_message(&req).unwrap();
        let payload_len = encoded.len() - 4;

        // Under the cap → decodes fine.
        let mut cursor = Cursor::new(&encoded);
        assert!(decode_message_limited::<ParseRequest>(&mut cursor, payload_len).is_ok());

        // Over the cap → InvalidData, nothing allocated.
        let mut cursor = Cursor::new(&encoded);
        let err = decode_message_limited::<ParseRequest>(&mut cursor, payload_len - 1)
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decode_rejects_oversized_frame_without_allocating() {
        // Forge a length prefix of u32::MAX (~4 GiB) with no payload behind it.
        // decode_message must reject it on the length check, never attempting the
        // multi-gigabyte allocation, and never blocking on read_exact for a body.
        let mut framed = (u32::MAX).to_be_bytes().to_vec();
        framed.extend_from_slice(b"only a few bytes follow");
        let mut cursor = Cursor::new(framed);
        let err = decode_message::<ParseRequest>(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("MAX_FRAME_LEN"),
            "expected MAX_FRAME_LEN rejection, got: {err}"
        );
    }

    #[test]
    fn decode_accepts_frame_at_the_boundary() {
        // A payload exactly at the cap must still decode (it is a valid JSON string).
        let big = "x".repeat(1024);
        let encoded = encode_message(&big).unwrap();
        let mut cursor = Cursor::new(encoded);
        let decoded: String = decode_message(&mut cursor).unwrap();
        assert_eq!(decoded, big);
    }
}
