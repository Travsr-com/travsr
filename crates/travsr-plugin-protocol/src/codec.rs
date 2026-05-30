use std::io::{self, Read, Write};

pub fn encode_message<T: serde::Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let payload = serde_json::to_vec(msg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "payload too large"))?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_message<T: serde::de::DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
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
            corpus: "github.com/acme/foo".into(),
            package: "acme".into(),
            source: None,
        };
        let encoded = encode_message(&req).unwrap();
        // Length prefix must be 4 bytes
        assert_eq!(encoded.len(), 4 + encoded.len() - 4);
        let mut cursor = Cursor::new(&encoded);
        let decoded: ParseRequest = decode_message(&mut cursor).unwrap();
        assert_eq!(decoded.path, req.path);
        assert_eq!(decoded.corpus, req.corpus);
    }
}
