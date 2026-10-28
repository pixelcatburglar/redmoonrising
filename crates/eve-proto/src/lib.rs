pub mod address;
pub mod error;
pub mod frame;
pub mod llv;
pub mod packet;

pub use address::MachoAddress;
pub use error::{ProtoError, Result};
pub use frame::{decompress, encode_frame, maybe_compress, FrameReader, MAX_FRAME_LEN};
pub use llv::VersionChallenge;
pub use packet::{MachoPacket, MessageType};

use eve_marshal::{StringTable, Value};

pub fn decode_payload(payload: &[u8], table: &StringTable) -> Result<Value> {
    let raw = decompress(payload)?;
    let value = eve_marshal::load(&raw, table)?;
    if let Value::Bytes(text) = &value {
        let (reason, bootver) = split_close_reason(text);
        return Err(ProtoError::PeerClosed { reason, bootver });
    }
    Ok(value)
}

pub fn encode_payload(value: &Value, table: &StringTable) -> Result<Vec<u8>> {
    let raw = eve_marshal::save(value, table)?;
    encode_frame(&raw)
}

fn split_close_reason(text: &[u8]) -> (String, Option<(f64, i64, Vec<u8>)>) {
    const MARKER: &[u8] = b", bootver=";
    let Some(pos) = text.windows(MARKER.len()).position(|w| w == MARKER) else {
        return (String::from_utf8_lossy(text).into_owned(), None);
    };
    let reason = String::from_utf8_lossy(&text[..pos]).into_owned();
    let hex = &text[pos + MARKER.len()..];
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.chunks(2) {
        let Ok(s) = std::str::from_utf8(pair) else { return (reason, None) };
        let Ok(b) = u8::from_str_radix(s, 16) else { return (reason, None) };
        bytes.push(b);
    }
    let parsed = eve_marshal::load(&bytes, &StringTable::empty()).ok().and_then(|v| {
        let items = v.seq()?;
        if items.len() != 3 {
            return None;
        }
        let version = match items[0] {
            Value::Double(d) => d,
            ref other => other.as_i64()? as f64,
        };
        Some((version, items[1].as_i64()?, items[2].as_bytes()?.to_vec()))
    });
    (reason, parsed)
}
