use crate::error::{Error, Result};
use crate::strings::StringTable;
use crate::tags;
use crate::value::Value;
use crate::{MAGIC, MAX_DEPTH};

pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
    readable: usize,
    map: &'a [u8],
    mapcount: usize,
    cursor: usize,
    shared: Vec<Option<Value>>,
    table: &'a StringTable,
    pub had_crc: bool,
    pub crc: Option<u32>,
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8], table: &'a StringTable) -> Result<Self> {
        if buf.len() < 5 {
            return Err(Error::UnexpectedEof { needed: 5, have: buf.len() });
        }
        debug_assert_eq!(buf[0], MAGIC);

        let mapcount = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]);
        let map_bytes = (mapcount as usize).saturating_mul(4);
        if map_bytes > buf.len().saturating_sub(5) {
            return Err(Error::TooLittleData { mapcount, len: buf.len() });
        }
        let readable = buf.len() - map_bytes;
        let map = &buf[readable..];

        for chunk in map.chunks_exact(4) {
            let e = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            if e == 0 || e as usize > mapcount as usize {
                return Err(Error::BogusMap);
            }
        }

        Ok(Decoder {
            buf,
            pos: 5,
            readable,
            map,
            mapcount: mapcount as usize,
            cursor: 0,
            shared: vec![None; mapcount as usize],
            table,
            had_crc: false,
            crc: None,
        })
    }

    fn remaining(&self) -> usize {
        self.readable.saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(Error::UnexpectedEof { needed: n, have: self.remaining() });
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn extsize(&mut self) -> Result<usize> {
        let b = self.u8()?;
        if b == 0xFF {
            Ok(self.u32()? as usize)
        } else {
            Ok(b as usize)
        }
    }

    fn take_slot(&mut self) -> Result<usize> {
        if self.cursor >= self.mapcount {
            return Err(Error::SharedTableOverflow);
        }
        let off = self.cursor * 4;
        let e = u32::from_le_bytes([
            self.map[off],
            self.map[off + 1],
            self.map[off + 2],
            self.map[off + 3],
        ]);
        self.cursor += 1;
        Ok(e as usize - 1)
    }

    pub fn root(&mut self) -> Result<Value> {
        let v = self.value()?;
        if self.remaining() != 0 {
            return Err(Error::TrailingData(self.remaining()));
        }
        Ok(v)
    }

    pub fn value(&mut self) -> Result<Value> {
        self.read(0)
    }

    fn read(&mut self, depth: u32) -> Result<Value> {
        if depth > MAX_DEPTH {
            return Err(Error::DepthLimit);
        }

        let (ty, shared) = loop {
            let tag = self.u8()?;
            let ty = tag & 0x3F;
            if ty == tags::CHECKSUM {
                self.crc = Some(self.u32()?);
                self.had_crc = true;
                continue;
            }
            break (ty, tag & 0x40 != 0);
        };

        if ty == tags::INSTANCE {
            let class = self.read(depth + 1)?;
            let slot = if shared { Some(self.take_slot()?) } else { None };
            if let Some(s) = slot {
                self.shared[s] = Some(Value::None);
            }
            let state = self.read(depth + 1)?;
            let v = Value::Instance { class: Box::new(class), state: Box::new(state) };
            if let Some(s) = slot {
                self.shared[s] = Some(v.clone());
            }
            return Ok(v);
        }

        let v = self.body(ty, depth)?;

        if shared {
            let slot = self.take_slot()?;
            self.shared[slot] = Some(v.clone());
        }
        Ok(v)
    }

    fn body(&mut self, ty: u8, depth: u32) -> Result<Value> {
        use tags::*;
        let v = match ty {
            NONE => Value::None,
            TRUE => Value::Bool(true),
            FALSE => Value::Bool(false),
            MARKER => Value::Marker,

            INT_MINUS1 => Value::Int(-1),
            INT_0 => Value::Int(0),
            INT_1 => Value::Int(1),
            INT8 => Value::Int(self.u8()? as i8 as i64),
            INT16 => {
                let b = self.take(2)?;
                Value::Int(i16::from_le_bytes([b[0], b[1]]) as i64)
            }
            INT32 => Value::Int(self.u32()? as i32 as i64),
            INT64 => {
                let b = self.take(8)?;
                let mut a = [0u8; 8];
                a.copy_from_slice(b);
                Value::Long(i64::from_le_bytes(a))
            }

            DOUBLE_0 => Value::Double(0.0),
            DOUBLE => {
                let b = self.take(8)?;
                let mut a = [0u8; 8];
                a.copy_from_slice(b);
                Value::Double(f64::from_le_bytes(a))
            }

            STRING_EMPTY => Value::Bytes(Vec::new()),
            STRING_CHAR => Value::Bytes(self.take(1)?.to_vec()),
            STRING_SHORT => {
                let n = self.u8()? as usize;
                Value::Bytes(self.take(n)?.to_vec())
            }
            STRING_LONG => {
                let n = self.extsize()?;
                Value::Bytes(self.take(n)?.to_vec())
            }
            STRING_TABLE => {
                let idx = self.u8()?;
                Value::Bytes(self.table.get(idx).unwrap_or(&[]).to_vec())
            }
            BUFFER => {
                let n = self.extsize()?;
                Value::Buffer(self.take(n)?.to_vec())
            }

            UNICODE_EMPTY => Value::Unicode(String::new()),
            UNICODE_CHAR => {
                let b = self.take(2)?;
                let u = u16::from_le_bytes([b[0], b[1]]);
                String::from_utf16(&[u]).map(Value::Unicode).map_err(|_| Error::BadUnicode)?
            }
            UNICODE_UCS2 => {
                let n = self.extsize()?;
                let b = self.take(n.checked_mul(2).ok_or(Error::BadUnicode)?)?;
                let units: Vec<u16> =
                    b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                String::from_utf16(&units).map(Value::Unicode).map_err(|_| Error::BadUnicode)?
            }
            UNICODE_UTF8 => {
                let n = self.extsize()?;
                let b = self.take(n)?;
                Value::Unicode(String::from_utf8(b.to_vec()).map_err(|_| Error::BadUnicode)?)
            }

            TUPLE_0 => Value::Tuple(Vec::new()),
            TUPLE_1 => Value::Tuple(vec![self.read(depth + 1)?]),
            TUPLE_2 => {
                let a = self.read(depth + 1)?;
                let b = self.read(depth + 1)?;
                Value::Tuple(vec![a, b])
            }
            TUPLE => {
                let n = self.extsize()?;
                Value::Tuple(self.items(n, depth)?)
            }

            LIST_0 => Value::List(Vec::new()),
            LIST_1 => Value::List(vec![self.read(depth + 1)?]),
            LIST => {
                let n = self.extsize()?;
                Value::List(self.items(n, depth)?)
            }

            DICT => {
                let n = self.extsize()?;
                self.guard_count(n)?;
                let mut pairs = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    let value = self.read(depth + 1)?;
                    let key = self.read(depth + 1)?;
                    pairs.push((key, value));
                }
                Value::Dict(pairs)
            }

            GLOBAL => {
                let n = self.extsize()?;
                Value::Global(self.take(n)?.to_vec())
            }

            CALLBACK_OBJECT => Value::CallbackObject(Box::new(self.read(depth + 1)?)),

            BLUE_OBJECT => {
                let n = self.extsize()?;
                Value::BlueObject(self.take(n)?.to_vec())
            }
            MARSHAL_STREAM => {
                let n = self.extsize()?;
                Value::MarshalStream(self.take(n)?.to_vec())
            }

            REFERENCE => {
                let n = self.extsize()? as u32;
                let idx = (n as usize).checked_sub(1).ok_or(Error::InvalidReference(n))?;
                self.shared
                    .get(idx)
                    .and_then(|s| s.clone())
                    .ok_or(Error::InvalidReference(n))?
            }

            PICKLE_SIZED => {
                let n = self.extsize()?;
                Value::Pickle(self.take(n)?.to_vec())
            }
            PICKLE_REST => {
                let n = self.remaining();
                Value::Pickle(self.take(n)?.to_vec())
            }

            DBROW => {
                return Err(Error::UnsupportedTag {
                    tag: DBROW,
                    why: "blue.DBRow packing is a separate subsystem and this era's \
                          service layer ships rows as util.Row / util.Rowset instead",
                })
            }
            0x22 | 0x23 => {
                return Err(Error::UnsupportedTag {
                    tag: ty,
                    why: "semantics not recovered; this client never emits it",
                })
            }

            _ => return Err(Error::InvalidTag(ty)),
        };
        Ok(v)
    }

    fn items(&mut self, n: usize, depth: u32) -> Result<Vec<Value>> {
        self.guard_count(n)?;
        let mut out = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            out.push(self.read(depth + 1)?);
        }
        Ok(out)
    }

    fn guard_count(&self, n: usize) -> Result<()> {
        if n > self.remaining() {
            return Err(Error::UnexpectedEof { needed: n, have: self.remaining() });
        }
        Ok(())
    }
}

pub fn load(buf: &[u8], table: &StringTable) -> Result<Value> {
    if buf.first() != Some(&MAGIC) {
        return Ok(Value::Pickle(buf.to_vec()));
    }
    Decoder::new(buf, table)?.root()
}

pub fn load_with_crc(buf: &[u8], table: &StringTable) -> Result<(Value, bool)> {
    if buf.first() != Some(&MAGIC) {
        return Ok((Value::Pickle(buf.to_vec()), false));
    }
    let mut d = Decoder::new(buf, table)?;
    let v = d.root()?;
    Ok((v, d.had_crc))
}
