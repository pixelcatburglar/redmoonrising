mod de;
mod error;
mod ser;
mod strings;
mod value;

pub use de::{load, load_with_crc, Decoder};
pub use error::{Error, Result};
pub use ser::{save, Encoder};
pub use strings::StringTable;
pub use value::Value;

pub const MAGIC: u8 = b'~';

pub const MAX_DEPTH: u32 = 64;

pub mod tags {
    pub const NONE: u8 = 0x01;
    pub const GLOBAL: u8 = 0x02;
    pub const INT64: u8 = 0x03;
    pub const INT32: u8 = 0x04;
    pub const INT16: u8 = 0x05;
    pub const INT8: u8 = 0x06;
    pub const INT_MINUS1: u8 = 0x07;
    pub const INT_0: u8 = 0x08;
    pub const INT_1: u8 = 0x09;
    pub const DOUBLE: u8 = 0x0A;
    pub const DOUBLE_0: u8 = 0x0B;
    pub const STRING_LONG: u8 = 0x0D;
    pub const STRING_EMPTY: u8 = 0x0E;
    pub const STRING_CHAR: u8 = 0x0F;
    pub const STRING_SHORT: u8 = 0x10;
    pub const STRING_TABLE: u8 = 0x11;
    pub const UNICODE_UCS2: u8 = 0x12;
    pub const BUFFER: u8 = 0x13;
    pub const TUPLE: u8 = 0x14;
    pub const LIST: u8 = 0x15;
    pub const DICT: u8 = 0x16;
    pub const INSTANCE: u8 = 0x17;
    pub const BLUE_OBJECT: u8 = 0x18;
    pub const CALLBACK_OBJECT: u8 = 0x19;
    pub const PICKLE_SIZED: u8 = 0x1A;
    pub const REFERENCE: u8 = 0x1B;
    pub const CHECKSUM: u8 = 0x1C;
    pub const TRUE: u8 = 0x1F;
    pub const FALSE: u8 = 0x20;
    pub const PICKLE_REST: u8 = 0x21;
    pub const TUPLE_0: u8 = 0x24;
    pub const TUPLE_1: u8 = 0x25;
    pub const LIST_0: u8 = 0x26;
    pub const LIST_1: u8 = 0x27;
    pub const UNICODE_EMPTY: u8 = 0x28;
    pub const UNICODE_CHAR: u8 = 0x29;
    pub const DBROW: u8 = 0x2A;
    pub const MARSHAL_STREAM: u8 = 0x2B;
    pub const TUPLE_2: u8 = 0x2C;
    pub const MARKER: u8 = 0x2D;
    pub const UNICODE_UTF8: u8 = 0x2E;
}
