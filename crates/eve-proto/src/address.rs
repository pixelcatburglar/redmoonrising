use crate::error::{ProtoError, Result};
use eve_marshal::Value;

pub const GUID: &[u8] = b"macho.MachoAddress";

#[derive(Clone, Debug, PartialEq)]
pub enum MachoAddress {
    Client { client_id: Option<i64>, call_id: Option<i64>, service: Option<Vec<u8>> },
    Node { node_id: Option<i64>, service: Option<Vec<u8>>, call_id: Option<i64> },
    Any { service: Option<Vec<u8>>, call_id: Option<i64> },
    Broadcast { broadcast_id: Value, narrowcast: Value, id_type: Value },
}

fn opt_i64(v: &Value) -> Result<Option<i64>> {
    match v {
        Value::None => Ok(None),
        other => other.as_i64().map(Some).ok_or(ProtoError::Shape("address integer")),
    }
}

fn opt_bytes(v: &Value) -> Result<Option<Vec<u8>>> {
    match v {
        Value::None => Ok(None),
        Value::Bytes(b) | Value::Buffer(b) => Ok(Some(b.clone())),
        _ => Err(ProtoError::Shape("address service name")),
    }
}

fn int(v: Option<i64>) -> Value {
    match v {
        Some(i) => Value::Int(i),
        None => Value::None,
    }
}

fn bytes(v: &Option<Vec<u8>>) -> Value {
    match v {
        Some(b) => Value::Bytes(b.clone()),
        None => Value::None,
    }
}

impl MachoAddress {
    pub fn call_id(&self) -> Option<i64> {
        match self {
            MachoAddress::Client { call_id, .. }
            | MachoAddress::Node { call_id, .. }
            | MachoAddress::Any { call_id, .. } => *call_id,
            MachoAddress::Broadcast { .. } => None,
        }
    }

    pub fn service(&self) -> Option<&[u8]> {
        match self {
            MachoAddress::Client { service, .. }
            | MachoAddress::Node { service, .. }
            | MachoAddress::Any { service, .. } => service.as_deref(),
            MachoAddress::Broadcast { .. } => None,
        }
    }

    pub fn to_tuple(&self) -> Value {
        match self {
            MachoAddress::Client { client_id, call_id, service } => Value::Tuple(vec![
                Value::bytes("C"),
                int(*client_id),
                int(*call_id),
                bytes(service),
            ]),
            MachoAddress::Node { node_id, service, call_id } => Value::Tuple(vec![
                Value::bytes("N"),
                int(*node_id),
                bytes(service),
                int(*call_id),
            ]),
            MachoAddress::Any { service, call_id } => {
                Value::Tuple(vec![Value::bytes("A"), bytes(service), int(*call_id)])
            }
            MachoAddress::Broadcast { broadcast_id, narrowcast, id_type } => Value::Tuple(vec![
                Value::bytes("B"),
                broadcast_id.clone(),
                narrowcast.clone(),
                id_type.clone(),
            ]),
        }
    }

    pub fn to_value(&self) -> Value {
        Value::Instance {
            class: Box::new(Value::Bytes(GUID.to_vec())),
            state: Box::new(self.to_tuple()),
        }
    }

    pub fn from_tuple(v: &Value) -> Result<Self> {
        let items = v.seq().ok_or(ProtoError::Shape("address tuple"))?;
        let tag = items
            .first()
            .and_then(|t| t.as_bytes())
            .ok_or(ProtoError::Shape("address tag"))?;
        match (tag, items.len()) {
            (b"C", 4) => Ok(MachoAddress::Client {
                client_id: opt_i64(&items[1])?,
                call_id: opt_i64(&items[2])?,
                service: opt_bytes(&items[3])?,
            }),
            (b"N", 4) => Ok(MachoAddress::Node {
                node_id: opt_i64(&items[1])?,
                service: opt_bytes(&items[2])?,
                call_id: opt_i64(&items[3])?,
            }),
            (b"A", 3) => Ok(MachoAddress::Any {
                service: opt_bytes(&items[1])?,
                call_id: opt_i64(&items[2])?,
            }),
            (b"B", 4) => Ok(MachoAddress::Broadcast {
                broadcast_id: items[1].clone(),
                narrowcast: items[2].clone(),
                id_type: items[3].clone(),
            }),
            _ => Err(ProtoError::Shape("address tag/arity")),
        }
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        match v {
            Value::Instance { class, state } => {
                if class.as_bytes() != Some(GUID) {
                    return Err(ProtoError::Shape("address guid"));
                }
                MachoAddress::from_tuple(state)
            }
            other => MachoAddress::from_tuple(other),
        }
    }
}
