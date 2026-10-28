use crate::address::MachoAddress;
use crate::error::{ProtoError, Result};
use eve_marshal::Value;

pub const MACHONETERR_UNMACHODESTINATION: i64 = 0;
pub const MACHONETERR_UNMACHOCHANNEL: i64 = 1;
pub const MACHONETERR_WRAPPEDEXCEPTION: i64 = 2;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MessageType {
    AuthenticationReq = 0,
    AuthenticationRsp = 1,
    IdentificationReq = 2,
    IdentificationRsp = 3,
    CallReq = 6,
    CallRsp = 7,
    TransportClosed = 8,
    ResolveReq = 10,
    ResolveRsp = 11,
    Notification = 12,
    ErrorResponse = 15,
    SessionChangeNotification = 16,
    SessionInitialStateNotification = 18,
    PingReq = 20,
    PingRsp = 21,
}

impl MessageType {
    pub fn from_i64(v: i64) -> Result<Self> {
        use MessageType::*;
        Ok(match v {
            0 => AuthenticationReq,
            1 => AuthenticationRsp,
            2 => IdentificationReq,
            3 => IdentificationRsp,
            6 => CallReq,
            7 => CallRsp,
            8 => TransportClosed,
            10 => ResolveReq,
            11 => ResolveRsp,
            12 => Notification,
            15 => ErrorResponse,
            16 => SessionChangeNotification,
            18 => SessionInitialStateNotification,
            20 => PingReq,
            21 => PingRsp,
            other => return Err(ProtoError::UnknownCommand(other)),
        })
    }

    pub fn code(self) -> i64 {
        self as i64
    }

    pub fn guid(self) -> &'static [u8] {
        use MessageType::*;
        match self {
            AuthenticationReq => b"macho.AuthenticationReq",
            AuthenticationRsp => b"macho.AuthenticationRsp",
            IdentificationReq => b"macho.IdentificationReq",
            IdentificationRsp => b"macho.IdentificationRsp",
            CallReq => b"macho.CallReq",
            CallRsp => b"macho.CallRsp",
            TransportClosed => b"macho.TransportClosed",
            ResolveReq => b"macho.ResolveReq",
            ResolveRsp => b"macho.ResolveRsp",
            Notification => b"macho.Notification",
            ErrorResponse => b"macho.ErrorResponse",
            SessionChangeNotification => b"macho.SessionChangeNotification",
            SessionInitialStateNotification => b"macho.SessionInitialStateNotification",
            PingReq => b"macho.PingReq",
            PingRsp => b"macho.PingRsp",
        }
    }

    pub fn name(self) -> &'static str {
        std::str::from_utf8(self.guid()).unwrap_or("macho.?")
    }

    pub fn channel(self) -> Option<&'static [u8]> {
        use MessageType::*;
        match self {
            AuthenticationReq | AuthenticationRsp => Some(b"authentication"),
            SessionChangeNotification | SessionInitialStateNotification => Some(b"sessionchange"),
            _ => None,
        }
    }

    pub fn body_arity(self) -> (&'static [usize], &'static str) {
        use MessageType::*;
        match self {
            AuthenticationReq => (&[3, 4], "3 or 4"),
            AuthenticationRsp => (&[4, 5], "4 or 5"),
            IdentificationReq | IdentificationRsp => (&[5], "5"),
            CallReq | Notification => (&[2], "2"),
            CallRsp => (&[1], "1"),
            TransportClosed | ResolveRsp => (&[2], "2"),
            ResolveReq => (&[0], "0"),
            ErrorResponse => (&[3], "3"),
            SessionChangeNotification => (&[1, 2], "1 or 2"),
            SessionInitialStateNotification => (&[1], "1"),
            PingReq | PingRsp => (&[1], "1"),
        }
    }

    pub fn is_request(self) -> bool {
        self.code() % 2 == 0
    }

    pub fn allowed_from_client(self) -> bool {
        use MessageType::*;
        matches!(
            self,
            ErrorResponse | Notification | CallRsp | CallReq | ResolveReq | AuthenticationReq | PingReq
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MachoPacket {
    pub command: MessageType,
    pub source: Option<MachoAddress>,
    pub destination: Option<MachoAddress>,
    pub user_id: Option<i64>,
    pub body: Vec<Value>,
    pub oob: Option<Vec<(Value, Value)>>,
}

impl MachoPacket {
    pub fn new(command: MessageType, body: Vec<Value>) -> Self {
        MachoPacket { command, source: None, destination: None, user_id: None, body, oob: None }
    }

    pub fn call_req(payload: Value) -> Self {
        Self::new(MessageType::CallReq, vec![payload, Value::None])
    }

    pub fn notification(payload: Value) -> Self {
        Self::new(MessageType::Notification, vec![payload, Value::None])
    }

    pub fn call_rsp(payload: Value) -> Self {
        Self::new(MessageType::CallRsp, vec![payload])
    }

    pub fn respond(&self, command: MessageType, body: Vec<Value>) -> Self {
        MachoPacket {
            command,
            source: self.destination.clone(),
            destination: self.source.clone(),
            user_id: self.user_id,
            body,
            oob: None,
        }
    }

    pub fn error_response(&self, code: i64, payload: Value) -> Self {
        self.respond(
            MessageType::ErrorResponse,
            vec![Value::Int(self.command.code()), Value::Int(code), payload],
        )
    }

    pub fn validate(&self) -> Result<()> {
        let (allowed, want) = self.command.body_arity();
        if !allowed.contains(&self.body.len()) {
            return Err(ProtoError::BadArity {
                command: self.command.name(),
                got: self.body.len(),
                want,
            });
        }
        Ok(())
    }

    fn oob_value(&self) -> Value {
        let mut dict = self.oob.clone().unwrap_or_default();
        if let Some(ch) = self.command.channel() {
            let key = Value::Bytes(b"channel".to_vec());
            if !dict.iter().any(|(k, _)| *k == key) {
                dict.push((key, Value::Bytes(ch.to_vec())));
            }
        }
        if dict.is_empty() {
            Value::None
        } else {
            Value::Dict(dict)
        }
    }

    pub fn to_value(&self) -> Result<Value> {
        self.validate()?;
        let addr = |a: &Option<MachoAddress>| match a {
            Some(a) => a.to_value(),
            None => Value::None,
        };
        let state = Value::Tuple(vec![
            Value::Int(self.command.code()),
            addr(&self.source),
            addr(&self.destination),
            match self.user_id {
                Some(u) => Value::Int(u),
                None => Value::None,
            },
            Value::Tuple(self.body.clone()),
            self.oob_value(),
        ]);
        Ok(Value::Instance {
            class: Box::new(Value::Bytes(self.command.guid().to_vec())),
            state: Box::new(state),
        })
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let (guid, state) = match v {
            Value::Instance { class, state } => (class.as_bytes().map(|b| b.to_vec()), &**state),
            other => (None, other),
        };
        let items = state.seq().ok_or(ProtoError::Shape("packet state tuple"))?;
        if items.len() != 6 {
            return Err(ProtoError::Shape("packet state tuple arity"));
        }

        let command = MessageType::from_i64(
            items[0].as_i64().ok_or(ProtoError::Shape("packet command"))?,
        )?;
        if let Some(g) = guid {
            if g != command.guid() {
                return Err(ProtoError::GuidMismatch {
                    command: command.name(),
                    guid: String::from_utf8_lossy(&g).into_owned(),
                });
            }
        }

        let addr = |v: &Value| -> Result<Option<MachoAddress>> {
            match v {
                Value::None => Ok(None),
                other => MachoAddress::from_value(other).map(Some),
            }
        };
        let user_id = match &items[3] {
            Value::None => None,
            other => Some(other.as_i64().ok_or(ProtoError::Shape("packet userID"))?),
        };
        let body = items[4].seq().ok_or(ProtoError::Shape("packet body"))?.to_vec();
        let oob = match &items[5] {
            Value::None => None,
            Value::Dict(d) => Some(d.clone()),
            _ => return Err(ProtoError::Shape("packet oob")),
        };

        Ok(MachoPacket {
            command,
            source: addr(&items[1])?,
            destination: addr(&items[2])?,
            user_id,
            body,
            oob,
        })
    }

    pub fn oob_get(&self, key: &str) -> Option<&Value> {
        let want = Value::Bytes(key.as_bytes().to_vec());
        self.oob.as_ref()?.iter().find(|(k, _)| *k == want).map(|(_, v)| v)
    }

    pub fn oob_set(&mut self, key: &str, value: Value) {
        let want = Value::Bytes(key.as_bytes().to_vec());
        let dict = self.oob.get_or_insert_with(Vec::new);
        if let Some(slot) = dict.iter_mut().find(|(k, _)| *k == want) {
            slot.1 = value;
        } else {
            dict.push((want, value));
        }
    }
}
