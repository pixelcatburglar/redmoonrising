use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    UnexpectedEof { needed: usize, have: usize },
    InvalidTag(u8),
    UnsupportedTag { tag: u8, why: &'static str },
    TooLittleData { mapcount: u32, len: usize },
    BogusMap,
    SharedTableOverflow,
    InvalidReference(u32),
    NoCallback,
    BadUnicode,
    DepthLimit,
    TrailingData(usize),
    ChecksumMismatch,
    Unencodable(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnexpectedEof { needed, have } => {
                write!(f, "unexpected end of stream: needed {needed} bytes, {have} remain")
            }
            Error::InvalidTag(t) => write!(f, "invalid type tag {} in stream", t & 0x3F),
            Error::UnsupportedTag { tag, why } => {
                write!(f, "unsupported type tag 0x{tag:02X}: {why}")
            }
            Error::TooLittleData { mapcount, len } => write!(
                f,
                "too little data in marshal stream, {len} bytes, mapcount in header is {mapcount}"
            ),
            Error::BogusMap => write!(f, "bogus map data in marshal stream"),
            Error::SharedTableOverflow => write!(f, "shared object table overflow"),
            Error::InvalidReference(n) => write!(f, "invalid shared reference {n} in stream"),
            Error::NoCallback => {
                write!(f, "stream contains custom data but no callback was supplied")
            }
            Error::BadUnicode => write!(f, "undecodable unicode payload"),
            Error::DepthLimit => write!(f, "nesting deeper than the configured limit"),
            Error::TrailingData(n) => write!(f, "{n} unconsumed bytes after the root object"),
            Error::ChecksumMismatch => write!(f, "checksum failed"),
            Error::Unencodable(what) => write!(f, "cannot encode {what}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
