use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum ProtoError {
    EmptyFrame,
    FrameTooLarge(u32),
    BadCompression,
    PeerClosed { reason: String, bootver: Option<(f64, i64, Vec<u8>)> },
    Marshal(eve_marshal::Error),
    Shape(&'static str),
    UnknownCommand(i64),
    GuidMismatch { command: &'static str, guid: String },
    BadArity { command: &'static str, got: usize, want: &'static str },
    VersionMismatch(&'static str),
    LlvTooLong(usize),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::EmptyFrame => write!(f, "incorrect size of message: 0"),
            ProtoError::FrameTooLarge(n) => write!(f, "incorrect size of message: {n}"),
            ProtoError::BadCompression => write!(f, "payload is neither marshal nor zlib"),
            ProtoError::PeerClosed { reason, .. } => write!(f, "peer closed: {reason}"),
            ProtoError::Marshal(e) => write!(f, "marshal: {e}"),
            ProtoError::Shape(what) => write!(f, "malformed {what}"),
            ProtoError::UnknownCommand(c) => write!(f, "unknown machoNet command {c}"),
            ProtoError::GuidMismatch { command, guid } => {
                write!(f, "command {command} arrived with guid {guid:?}")
            }
            ProtoError::BadArity { command, got, want } => {
                write!(f, "{command} body has {got} elements, expected {want}")
            }
            ProtoError::VersionMismatch(why) => {
                write!(f, "there was a low-level protocol version mismatch: {why}")
            }
            ProtoError::LlvTooLong(n) => {
                write!(f, "a low-level-version challenge response may not exceed 1000 bytes (got {n})")
            }
        }
    }
}

impl std::error::Error for ProtoError {}

impl From<eve_marshal::Error> for ProtoError {
    fn from(e: eve_marshal::Error) -> Self {
        ProtoError::Marshal(e)
    }
}

pub type Result<T> = std::result::Result<T, ProtoError>;
