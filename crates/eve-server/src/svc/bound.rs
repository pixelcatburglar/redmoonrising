use eve_marshal::{StringTable, Value};
use tracing::{error, info, warn};

use crate::services::{Call, Outcome};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound<'a> {
    pub service: &'a str,
    pub params: Vec<i64>,
}


pub fn object_id(node_id: i64, service: &str, params: &[i64]) -> String {
    let mut s = format!("N={node_id}:{service}#");
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&p.to_string());
    }
    s
}

pub fn parse(service: &str) -> Option<Bound<'_>> {
    let (node, rest) = service.split_once(':')?;
    if !node.starts_with("N=") {
        return None;
    }
    let (name, params) = match rest.split_once('#') {
        Some((n, p)) => (n, p),
        None => (rest, ""),
    };
    Some(Bound {
        service: name,
        params: params.split(',').filter_map(|p| p.parse::<i64>().ok()).collect(),
    })
}

pub fn owns(service: &str, want: &str) -> bool {
    service == want || parse(service).is_some_and(|b| b.service == want)
}

pub fn params_of(call: &Call) -> Vec<i64> {
    if let Some(b) = parse(&call.service) {
        return b.params;
    }
    match call.args.first() {
        Some(Value::Tuple(v)) | Some(Value::List(v)) => {
            v.iter().filter_map(|x| x.as_i64()).collect()
        }
        Some(v) => v.as_i64().into_iter().collect(),
        None => Vec::new(),
    }
}

pub fn object_ref(node_id: i64, service: &str, params: &[i64]) -> Value {
    object_ref_with(node_id, service, params, Vec::new())
}

/// A reference to a server-side object, carrying the public attributes the
/// client reads off it without a call (`__publicattributes__`;
/// ObjectCallGPCS pickles them as `(objectID, attributes, refID)`).
pub fn object_ref_with(node_id: i64, service: &str, params: &[i64], attributes: Vec<(&str, Value)>) -> Value {
    let id = Value::Bytes(object_id(node_id, service, params).into_bytes());
    let ref_id = Value::Long(crate::bulkdata::filetime_now());
    let inner = if attributes.is_empty() {
        Value::Tuple(vec![id, ref_id])
    } else {
        let attributes = attributes.into_iter().map(|(k, v)| (Value::bytes(k), v)).collect();
        Value::Tuple(vec![id, Value::Dict(attributes), ref_id])
    };
    match eve_marshal::save(&inner, &StringTable::empty()) {
        Ok(blob) => Value::CallbackObject(Box::new(Value::Bytes(blob))),
        Err(e) => {
            error!("could not marshal an object ref for {service}: {err}", err = crate::logging::chain(&e));
            Value::None
        }
    }
}

pub fn piggyback(call: &Call) -> Option<(String, Vec<Value>)> {
    let parts = call.args.get(1).and_then(|v| v.seq())?;
    let method = parts
        .first()
        .and_then(|m| m.as_bytes())
        .map(|b| String::from_utf8_lossy(b).into_owned())?;
    let args = parts.get(1).and_then(|a| a.seq()).map(|s| s.to_vec()).unwrap_or_default();
    Some((method, args))
}

pub fn bind_result(node_id: i64, service: &str, params: &[i64], retval: Value) -> Value {
    Value::Tuple(vec![object_ref(node_id, service, params), retval])
}

pub fn piggyback_value(service: &str, outcome: Outcome) -> Value {
    match outcome {
        Outcome::Value(v) => v,
        Outcome::Unknown(what) => {
            warn!("{service}: bind carried an unknown call: {what}");
            Value::None
        }
        Outcome::UserError { msg, .. } => {
            info!("{service}: bind carried a call that refused: {msg}");
            Value::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_an_object_id() {
        let id = object_id(1, "invbroker", &[60_000_004, 15]);
        assert_eq!(id, "N=1:invbroker#60000004,15");
        let b = parse(&id).expect("decodes");
        assert_eq!(b.service, "invbroker");
        assert_eq!(b.params, vec![60_000_004, 15]);
    }

    #[test]
    fn object_id_has_exactly_one_colon() {
        assert_eq!(object_id(7, "corpRegistry", &[1_000_002, 1]).matches(':').count(), 1);
        assert_eq!(object_id(7, "skillMgr", &[]).matches(':').count(), 1);
    }

    #[test]
    fn owns_matches_name_and_id() {
        assert!(owns("corpRegistry", "corpRegistry"));
        assert!(owns("N=1:corpRegistry#1000002,1", "corpRegistry"));
        assert!(!owns("N=1:corpRegistry#1000002,1", "invbroker"));
        assert!(!owns("corpmgr", "corpRegistry"));
    }

    #[test]
    fn parses_a_paramless_object() {
        let b = parse("N=1:warRegistry#").expect("decodes");
        assert_eq!(b.service, "warRegistry");
        assert!(b.params.is_empty());
    }
}
