//! Service calls: parsing a CallReq, the context a handler runs in, and the
//! registry that routes a call to the handlers for its service.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use eve_marshal::{StringTable, Value};
use eve_proto::{MachoPacket, ProtoError};
use tokio_postgres::types::ToSql;
use tokio_postgres::Row;
use tracing::error;

use crate::bulkdata::BulkData;
use crate::session::Session;
use crate::world::World;
use crate::svc::{bound, rows};

pub struct Call {
    /// The service name, or for a bound object its full ID (`N=1:invbroker#60000004`).
    pub service: String,
    pub method: String,
    pub args: Vec<Value>,
    pub kwargs: Vec<(Value, Value)>,
}

impl Call {
    /// The service name with any bound-object ID stripped.
    pub fn service_name(&self) -> &str {
        bound::parse(&self.service).map_or(self.service.as_str(), |b| b.service)
    }

    pub fn arg_i64(&self, i: usize) -> Option<i64> {
        self.args.get(i).and_then(|v| v.as_i64())
    }

    /// An ID argument; 0 when missing or not an integer.
    pub fn arg_id(&self, i: usize) -> i64 {
        self.arg_i64(i).unwrap_or(0)
    }

    pub fn arg_text(&self, i: usize) -> Option<String> {
        self.args.get(i).and_then(|v| v.as_string())
    }

    pub fn kwarg(&self, name: &str) -> Option<&Value> {
        let key = Value::bytes(name);
        self.kwargs.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }
}

pub fn parse_call(packet: &MachoPacket, table: &StringTable) -> Result<Call, ProtoError> {
    let payload = packet.body.first().ok_or(ProtoError::Shape("call body"))?;
    let items = payload.seq().ok_or(ProtoError::Shape("call payload"))?;
    let bound = items.first().and_then(|v| v.as_i64()) == Some(1);
    let blob = match items.get(1) {
        Some(Value::MarshalStream(b)) | Some(Value::Bytes(b)) | Some(Value::Buffer(b)) => b,
        _ => return Err(ProtoError::Shape("call payload blob")),
    };

    let inner = eve_marshal::load(blob, table)?;
    let parts = inner.seq().ok_or(ProtoError::Shape("inner call tuple"))?;

    let service = packet
        .destination
        .as_ref()
        .and_then(|d| d.service())
        .or_else(|| if bound { parts.first().and_then(|v| v.as_bytes()) } else { None })
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .ok_or(ProtoError::Shape("call destination has no service"))?;

    let method = parts
        .get(1)
        .and_then(|m| m.as_bytes())
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .ok_or(ProtoError::Shape("call method name"))?;
    let args = parts.get(2).and_then(|a| a.seq()).map(|s| s.to_vec()).unwrap_or_default();
    let mut kwargs = match parts.get(3) {
        Some(Value::Dict(d)) => d.clone(),
        _ => Vec::new(),
    };
    // machoVersion is transport metadata, not an argument of the method.
    kwargs.retain(|(k, _)| *k != Value::bytes("machoVersion"));

    Ok(Call { service, method, args, kwargs })
}

pub fn make_response(
    req: &MachoPacket,
    value: &Value,
    table: &StringTable,
) -> Result<MachoPacket, ProtoError> {
    let blob = eve_marshal::save(value, table)?;
    Ok(req.respond(eve_proto::MessageType::CallRsp, vec![Value::Bytes(blob)]))
}

pub enum Outcome {
    Value(Value),
    /// Not implemented; the text says what, and goes back as ServiceNotFound.
    Unknown(String),
    UserError { msg: String, args: Vec<(String, Value)> },
}

impl Outcome {
    /// A `CustomError` refusal: the client shows `text` as is.
    pub fn custom(text: impl Into<String>) -> Self {
        Outcome::UserError {
            msg: "CustomError".into(),
            args: vec![("error".into(), Value::bytes(text.into()))],
        }
    }

    /// `NotEnoughMoney`, for a payment of `need` from a wallet holding `have`.
    pub fn not_enough_money(need: f64, have: f64) -> Self {
        Outcome::UserError {
            msg: "NotEnoughMoney".into(),
            args: vec![("amount".into(), Value::Double(need)), ("balance".into(), Value::Double(have))],
        }
    }
}

pub struct Ctx<'a> {
    pub bulk: &'a BulkData,
    pub dogma: &'a crate::dogma::Catalog,
    pub session: &'a Session,
    pub db: &'a tokio_postgres::Client,
    pub db_url: &'a str,
    pub node_id: i64,
    pub world: &'a World,
    /// The caller's connection, to queue notifications for it.
    pub client_id: i64,
}

type Params<'p> = &'p [&'p (dyn ToSql + Sync)];

impl Ctx<'_> {
    /// Rows of a query. A database error is logged (the call span says which call)
    /// and yields no rows, so the client gets an empty answer rather than an error.
    pub async fn query(&self, sql: &str, params: Params<'_>) -> Vec<Row> {
        match self.db.query(sql, params).await {
            Ok(rows) => rows,
            Err(e) => {
                error!("query failed: {}", crate::logging::chain(&e));
                Vec::new()
            }
        }
    }

    /// The first row of a query, if any. Errors as for [`Ctx::query`].
    pub async fn query_one(&self, sql: &str, params: Params<'_>) -> Option<Row> {
        self.query(sql, params).await.into_iter().next()
    }

    /// A query whose columns are `header`, as a `util.Rowset`.
    pub async fn rowset(&self, header: &[&str], sql: &str, params: Params<'_>) -> Value {
        rows::rowset(header, rows::lines(&self.query(sql, params).await))
    }

    /// Runs a statement; false (and logged) if it failed.
    pub async fn execute(&self, sql: &str, params: Params<'_>) -> bool {
        match self.db.execute(sql, params).await {
            Ok(_) => true,
            Err(e) => {
                error!("statement failed: {}", crate::logging::chain(&e));
                false
            }
        }
    }
}

pub type HandlerFuture<'a> = Pin<Box<dyn Future<Output = Option<Outcome>> + Send + 'a>>;

/// Serves the methods of one service it knows, and returns `None` for the rest.
pub type Handler = for<'a> fn(&'a Call, &'a Ctx<'a>) -> HandlerFuture<'a>;

/// Turn an `async fn(&Call, &Ctx) -> Option<Outcome>` into a [`Handler`].
macro_rules! handler {
    ($f:path) => {{
        let h: $crate::services::Handler = |call, ctx| Box::pin($f(call, ctx));
        h
    }};
}
pub(crate) use handler;

struct Service {
    name: &'static str,
    bound: bool,
    handlers: Vec<Handler>,
}

impl Service {
    async fn call(&self, call: &Call, ctx: &Ctx<'_>) -> Outcome {
        for handler in &self.handlers {
            if let Some(outcome) = handler(call, ctx).await {
                return outcome;
            }
        }
        Outcome::Unknown(format!("{}::{}", self.name, call.method))
    }

    /// `MachoBindObject(params, (method, args))`: hand out an object reference and,
    /// if the client piggybacked a call on the bind, its result.
    async fn bind(&self, call: &Call, ctx: &Ctx<'_>) -> Outcome {
        let params = bound::params_of(call);
        let retval = match bound::piggyback(call) {
            Some((method, args)) => {
                let inner = Call {
                    service: bound::object_id(ctx.node_id, self.name, &params),
                    method,
                    args,
                    kwargs: Vec::new(),
                };
                bound::piggyback_value(self.name, self.call(&inner, ctx).await)
            }
            None => Value::None,
        };
        Outcome::Value(bound::bind_result(ctx.node_id, self.name, &params, retval))
    }
}

/// Every service the server answers, by name.
#[derive(Default)]
pub struct Registry {
    services: HashMap<&'static str, Service>,
}

impl Registry {
    /// Add a service. Several handlers may share a name; they are tried in the
    /// order they were added.
    pub fn add(&mut self, name: &'static str, handler: Handler) -> &mut Self {
        self.entry(name, false).handlers.push(handler);
        self
    }

    /// Add a bound service: the client binds an object with `MachoResolveObject` /
    /// `MachoBindObject` before calling methods on it. The registry answers those two.
    pub fn add_bound(&mut self, name: &'static str, handler: Handler) -> &mut Self {
        self.entry(name, true).handlers.push(handler);
        self
    }

    fn entry(&mut self, name: &'static str, bound: bool) -> &mut Service {
        let svc = self
            .services
            .entry(name)
            .or_insert_with(|| Service { name, bound, handlers: Vec::new() });
        assert_eq!(svc.bound, bound, "service {name} registered as both bound and unbound");
        svc
    }

    pub fn len(&self) -> usize {
        self.services.len()
    }

    pub async fn dispatch(&self, call: &Call, ctx: &Ctx<'_>) -> Outcome {
        let name = call.service_name();
        let Some(svc) = self.services.get(name) else {
            return Outcome::Unknown(format!("{name}::{} (no such service)", call.method));
        };
        if svc.bound {
            match call.method.as_str() {
                "MachoResolveObject" => return Outcome::Value(Value::Long(ctx.node_id)),
                "MachoBindObject" => return svc.bind(call, ctx).await,
                _ => {}
            }
        }
        svc.call(call, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_proto::{MachoAddress, MessageType};

    fn call_req(destination: MachoAddress, flag: i64, inner: Value) -> MachoPacket {
        let table = StringTable::empty();
        let blob = eve_marshal::save(&inner, &table).expect("inner encodes");
        let mut p = MachoPacket::new(
            MessageType::CallReq,
            vec![
                Value::Tuple(vec![Value::Int(flag), Value::Bytes(blob)]),
                Value::None,
            ],
        );
        p.destination = Some(destination);
        p
    }

    #[test]
    fn parses_a_plain_service_call() {
        let packet = call_req(
            MachoAddress::Any { service: Some(b"config".to_vec()), call_id: Some(7) },
            0,
            Value::Tuple(vec![
                Value::Int(1),
                Value::bytes("GetMultiOwnersEx"),
                Value::Tuple(vec![Value::List(vec![Value::Int(140_000_001)])]),
                Value::Dict(vec![(Value::bytes("machoVersion"), Value::Int(1))]),
            ]),
        );
        let call = parse_call(&packet, &StringTable::empty()).expect("parses");
        assert_eq!(call.service, "config");
        assert_eq!(call.method, "GetMultiOwnersEx");
        assert_eq!(call.args.len(), 1);
        assert!(call.kwargs.is_empty());
    }

    #[test]
    fn parses_a_bound_object_call() {
        let packet = call_req(
            MachoAddress::Node { node_id: Some(1), service: None, call_id: Some(9) },
            1,
            Value::Tuple(vec![
                Value::bytes("N=1:invcontainer#0,10004,-1"),
                Value::bytes("List"),
                Value::Tuple(Vec::new()),
                Value::Dict(Vec::new()),
            ]),
        );
        let call = parse_call(&packet, &StringTable::empty()).expect("parses");
        assert_eq!(call.service, "N=1:invcontainer#0,10004,-1");
        assert_eq!(call.service_name(), "invcontainer");
        assert_eq!(call.method, "List");
        let b = crate::svc::bound::parse(&call.service).expect("decodes");
        assert_eq!(b.params, vec![0, 10004, -1]);
    }

    #[test]
    fn destination_service_beats_the_object_id() {
        let packet = call_req(
            MachoAddress::Node {
                node_id: Some(1),
                service: Some(b"corpRegistry".to_vec()),
                call_id: None,
            },
            1,
            Value::Tuple(vec![
                Value::bytes("N=1:invcontainer#0,10004,-1"),
                Value::bytes("GetCorporation"),
                Value::Tuple(Vec::new()),
                Value::Dict(Vec::new()),
            ]),
        );
        let call = parse_call(&packet, &StringTable::empty()).expect("parses");
        assert_eq!(call.service, "corpRegistry");
    }

    #[test]
    fn the_registry_builds_without_conflicts() {
        assert!(crate::svc::registry().len() > 20);
    }
}
