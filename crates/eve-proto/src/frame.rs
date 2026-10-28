use crate::error::{ProtoError, Result};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::{Read, Write};

pub const PREFIX_LEN: usize = 4;

/// The most we send in one frame: the client drops anything larger.
pub const MAX_FRAME_LEN: u32 = 1_000_000;

/// The most we accept in one frame. The client sends more than MAX_FRAME_LEN
/// at times (portrait uploads, long error reports); this only guards memory.
pub const MAX_INCOMING_FRAME_LEN: u32 = 16_000_000;

pub const MARSHAL_MAGIC: u8 = b'~';

const COMPRESSION_LEVEL: u32 = 1;

#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        FrameReader { buf: Vec::with_capacity(2048) }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn buffered(&self) -> usize {
        self.buf.len()
    }

    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>> {
        if self.buf.len() < PREFIX_LEN {
            return Ok(None);
        }
        let len = u32::from_le_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
        if len == 0 {
            return Err(ProtoError::EmptyFrame);
        }
        if len > MAX_INCOMING_FRAME_LEN {
            return Err(ProtoError::FrameTooLarge(len));
        }
        let total = PREFIX_LEN + len as usize;
        if self.buf.len() < total {
            return Ok(None);
        }
        let payload = self.buf[PREFIX_LEN..total].to_vec();
        self.buf.drain(..total);
        Ok(Some(payload))
    }
}

pub fn encode_frame(payload: &[u8]) -> Result<Vec<u8>> {
    if payload.is_empty() {
        return Err(ProtoError::EmptyFrame);
    }
    if payload.len() as u64 > MAX_FRAME_LEN as u64 {
        return Err(ProtoError::FrameTooLarge(payload.len() as u32));
    }
    let mut out = Vec::with_capacity(PREFIX_LEN + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

pub fn decompress(payload: &[u8]) -> Result<Vec<u8>> {
    if payload.first() == Some(&MARSHAL_MAGIC) {
        return Ok(payload.to_vec());
    }
    let mut out = Vec::with_capacity(payload.len() * 3);
    ZlibDecoder::new(payload)
        .read_to_end(&mut out)
        .map_err(|_| ProtoError::BadCompression)?;
    if out.first() != Some(&MARSHAL_MAGIC) {
        return Err(ProtoError::BadCompression);
    }
    Ok(out)
}

pub fn maybe_compress(payload: &[u8], compressed_part: usize) -> Vec<u8> {
    if payload.is_empty() {
        return payload.to_vec();
    }
    if compressed_part * 100 / payload.len() >= 75 {
        return payload.to_vec();
    }
    if payload.len().saturating_sub(compressed_part) <= 200 {
        return payload.to_vec();
    }
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::new(COMPRESSION_LEVEL));
    if enc.write_all(payload).is_err() {
        return payload.to_vec();
    }
    let Ok(compressed) = enc.finish() else {
        return payload.to_vec();
    };
    if compressed.len() >= payload.len() {
        return payload.to_vec();
    }
    let gain = (payload.len() - compressed.len()) * 100 / payload.len();
    if gain <= 5 {
        return payload.to_vec();
    }
    compressed
}
