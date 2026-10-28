use crate::error::{Error, Result};
use crate::strings::StringTable;
use crate::tags;
use crate::value::Value;
use crate::{MAGIC, MAX_DEPTH};

pub struct Encoder<'a> {
    out: Vec<u8>,
    table: &'a StringTable,
}

impl<'a> Encoder<'a> {
    pub fn new(table: &'a StringTable) -> Self {
        Encoder { out: vec![MAGIC, 0, 0, 0, 0], table }
    }

    pub fn finish(self) -> Vec<u8> {
        self.out
    }

    fn u8(&mut self, b: u8) {
        self.out.push(b);
    }

    fn raw(&mut self, b: &[u8]) {
        self.out.extend_from_slice(b);
    }

    fn extsize(&mut self, n: usize) {
        if n < 0xFF {
            self.out.push(n as u8);
        } else {
            self.out.push(0xFF);
            self.out.extend_from_slice(&(n as u32).to_le_bytes());
        }
    }

    pub fn write(&mut self, v: &Value) -> Result<()> {
        self.write_at(v, 0)
    }

    fn write_at(&mut self, v: &Value, depth: u32) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(Error::DepthLimit);
        }
        match v {
            Value::None => self.u8(tags::NONE),
            Value::Bool(true) => self.u8(tags::TRUE),
            Value::Bool(false) => self.u8(tags::FALSE),
            Value::Marker => self.u8(tags::MARKER),

            Value::Int(i) => match *i {
                -1 => self.u8(tags::INT_MINUS1),
                0 => self.u8(tags::INT_0),
                1 => self.u8(tags::INT_1),
                v if (-128..=127).contains(&v) => {
                    self.u8(tags::INT8);
                    self.u8(v as i8 as u8);
                }
                v if (-32768..=32767).contains(&v) => {
                    self.u8(tags::INT16);
                    self.raw(&(v as i16).to_le_bytes());
                }
                v if (i32::MIN as i64..=i32::MAX as i64).contains(&v) => {
                    self.u8(tags::INT32);
                    self.raw(&(v as i32).to_le_bytes());
                }
                v => {
                    self.u8(tags::INT64);
                    self.raw(&v.to_le_bytes());
                }
            },
            Value::Long(i) => {
                self.u8(tags::INT64);
                self.raw(&i.to_le_bytes());
            }

            Value::Double(d) => {
                if *d == 0.0 && d.is_sign_positive() {
                    self.u8(tags::DOUBLE_0);
                } else {
                    self.u8(tags::DOUBLE);
                    self.raw(&d.to_le_bytes());
                }
            }

            Value::Bytes(b) => self.write_str(b),
            Value::Buffer(b) => {
                self.u8(tags::BUFFER);
                self.extsize(b.len());
                self.raw(b);
            }
            Value::Unicode(s) => self.write_unicode(s),

            Value::Tuple(items) => {
                match items.len() {
                    0 => self.u8(tags::TUPLE_0),
                    1 => self.u8(tags::TUPLE_1),
                    2 => self.u8(tags::TUPLE_2),
                    n => {
                        self.u8(tags::TUPLE);
                        self.extsize(n);
                    }
                }
                for it in items {
                    self.write_at(it, depth + 1)?;
                }
            }
            Value::List(items) => {
                match items.len() {
                    0 => self.u8(tags::LIST_0),
                    1 => self.u8(tags::LIST_1),
                    n => {
                        self.u8(tags::LIST);
                        self.extsize(n);
                    }
                }
                for it in items {
                    self.write_at(it, depth + 1)?;
                }
            }
            Value::Dict(pairs) => {
                self.u8(tags::DICT);
                self.extsize(pairs.len());
                for (k, val) in pairs {
                    self.write_at(val, depth + 1)?;
                    self.write_at(k, depth + 1)?;
                }
            }

            Value::Global(name) => {
                self.u8(tags::GLOBAL);
                self.extsize(name.len());
                self.raw(name);
            }
            Value::Instance { class, state } => {
                self.u8(tags::INSTANCE);
                self.write_at(class, depth + 1)?;
                self.write_at(state, depth + 1)?;
            }
            Value::CallbackObject(id) => {
                self.u8(tags::CALLBACK_OBJECT);
                self.write_at(id, depth + 1)?;
            }
            Value::BlueObject(b) => {
                self.u8(tags::BLUE_OBJECT);
                self.extsize(b.len());
                self.raw(b);
            }
            Value::MarshalStream(blob) => {
                self.u8(tags::MARSHAL_STREAM);
                self.extsize(blob.len());
                self.raw(blob);
            }
            Value::Pickle(b) => {
                self.u8(tags::PICKLE_REST);
                self.raw(b);
            }
        }
        Ok(())
    }

    fn write_str(&mut self, b: &[u8]) {
        if let Some(idx) = self.table.index_of(b) {
            self.u8(tags::STRING_TABLE);
            self.u8(idx);
        } else if b.is_empty() {
            self.u8(tags::STRING_EMPTY);
        } else if b.len() == 1 {
            self.u8(tags::STRING_CHAR);
            self.u8(b[0]);
        } else if b.len() < 256 {
            self.u8(tags::STRING_SHORT);
            self.u8(b.len() as u8);
            self.raw(b);
        } else {
            self.u8(tags::STRING_LONG);
            self.extsize(b.len());
            self.raw(b);
        }
    }

    fn write_unicode(&mut self, s: &str) {
        let units: Vec<u16> = s.encode_utf16().collect();
        match units.len() {
            0 => self.u8(tags::UNICODE_EMPTY),
            1 => {
                self.u8(tags::UNICODE_CHAR);
                self.raw(&units[0].to_le_bytes());
            }
            n => {
                if s.len() < 2 * n {
                    self.u8(tags::UNICODE_UTF8);
                    self.extsize(s.len());
                    self.raw(s.as_bytes());
                } else {
                    self.u8(tags::UNICODE_UCS2);
                    self.extsize(n);
                    for u in &units {
                        self.raw(&u.to_le_bytes());
                    }
                }
            }
        }
    }
}

pub fn save(v: &Value, table: &StringTable) -> Result<Vec<u8>> {
    let mut e = Encoder::new(table);
    e.write(v)?;
    Ok(e.finish())
}
