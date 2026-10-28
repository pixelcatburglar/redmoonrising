#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    None,
    Bool(bool),
    Int(i64),
    Long(i64),
    Double(f64),
    Bytes(Vec<u8>),
    Buffer(Vec<u8>),
    Unicode(String),
    Tuple(Vec<Value>),
    List(Vec<Value>),
    Dict(Vec<(Value, Value)>),
    Global(Vec<u8>),
    Instance {
        class: Box<Value>,
        state: Box<Value>,
    },
    CallbackObject(Box<Value>),
    BlueObject(Vec<u8>),
    MarshalStream(Vec<u8>),
    Pickle(Vec<u8>),
    Marker,
}

impl Value {
    pub fn bytes(s: impl Into<Vec<u8>>) -> Value {
        Value::Bytes(s.into())
    }

    pub fn unicode(s: impl Into<String>) -> Value {
        Value::Unicode(s.into())
    }

    /// Text for the client: plain ASCII as `str`, anything else as `unicode`, which
    /// the client renders correctly where UTF-8 bytes would come out garbled.
    pub fn text(s: impl Into<String>) -> Value {
        let s = s.into();
        if s.is_ascii() {
            Value::Bytes(s.into_bytes())
        } else {
            Value::Unicode(s)
        }
    }

    /// An `int`, or `None` when absent.
    pub fn int_or_none(v: Option<i64>) -> Value {
        v.map_or(Value::None, Value::Int)
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::None => "NoneType",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Long(_) => "long",
            Value::Double(_) => "float",
            Value::Bytes(_) => "str",
            Value::Buffer(_) => "buffer",
            Value::Unicode(_) => "unicode",
            Value::Tuple(_) => "tuple",
            Value::List(_) => "list",
            Value::Dict(_) => "dict",
            Value::Global(_) => "global",
            Value::Instance { .. } => "instance",
            Value::CallbackObject(_) => "callback",
            Value::BlueObject(_) => "blue object",
            Value::MarshalStream(_) => "blue.MarshalStream",
            Value::Pickle(_) => "pickle",
            Value::Marker => "marker",
        }
    }

    pub fn get(&self, key: &Value) -> Option<&Value> {
        match self {
            Value::Dict(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_str(&self, key: &str) -> Option<&Value> {
        self.get(&Value::Bytes(key.as_bytes().to_vec()))
    }

    pub fn seq(&self) -> Option<&[Value]> {
        match self {
            Value::Tuple(v) | Value::List(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Value::Int(v) | Value::Long(v) => Some(v),
            Value::Bool(b) => Some(b as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Value::Double(v) => Some(v),
            _ => self.as_i64().map(|v| v as f64),
        }
    }

    /// A `str`, `buffer` or `unicode` as a Rust string (bytes decoded lossily as UTF-8).
    pub fn as_string(&self) -> Option<String> {
        match self {
            Value::Bytes(b) | Value::Buffer(b) => Some(String::from_utf8_lossy(b).into_owned()),
            Value::Unicode(s) => Some(s.clone()),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) | Value::Buffer(b) => Some(b),
            _ => None,
        }
    }
}
