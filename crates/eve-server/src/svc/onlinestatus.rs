use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::rowset;

const HEADER: &[&str] = &["characterID", "online"];

pub async fn online_status(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetInitialState" => initial_state(ctx),
        "GetOnlineStatus" => {
            is_online(ctx, call.arg_id(0))
        }
        _ => return None,
    };
    Some(Outcome::Value(value))
}

fn initial_state(ctx: &Ctx<'_>) -> Value {
    let lines = match ctx.session.char_id {
        Some(id) => vec![Value::List(vec![Value::Int(id), Value::Bool(true)])],
        None => Vec::new(),
    };
    rowset(HEADER, lines)
}

fn is_online(ctx: &Ctx<'_>, char_id: i64) -> Value {
    Value::Bool(ctx.world.lock().is_online(char_id))
}
