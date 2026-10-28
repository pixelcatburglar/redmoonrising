use std::collections::HashMap;
use std::sync::Mutex;

use eve_marshal::Value;
use tracing::{debug, warn};

use crate::services::{Call, Ctx, Outcome};

pub async fn alert(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "BeanCount" => bean_count(call),
        "SendClientStackTraceAlert" => client_stack_trace(call, ctx),
        "BeanDelivery" => bean_delivery(call),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

struct Bean {
    error_id: i64,
    key: String,
    occurrences: i64,
}

static BEANS: Mutex<Option<HashMap<i64, Bean>>> = Mutex::new(None);

fn with_beans<T>(f: impl FnOnce(&mut HashMap<i64, Bean>) -> T) -> T {
    let mut guard = match BEANS.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    f(guard.get_or_insert_with(HashMap::new))
}

fn beans_answer(error_id: i64) -> Value {
    Value::Tuple(vec![Value::Int(error_id), Value::Int(LOGGING_BATCHED)])
}

const LOGGING_BATCHED: i64 = 0;

fn bean_count(call: &Call) -> Value {
    let Some(hash) = call.arg_i64(0) else {
        warn!("BeanCount with no usable stack hash; asking for the trace");
        return Value::Tuple(vec![Value::None, Value::None]);
    };
    with_beans(|beans| match beans.get_mut(&hash) {
        Some(bean) => {
            bean.occurrences += 1;
            beans_answer(bean.error_id)
        }
        None => Value::Tuple(vec![Value::None, Value::None]),
    })
}

fn client_stack_trace(call: &Call, ctx: &Ctx<'_>) -> Value {
    let stack_id = call.args.first();
    let hash = stack_id
        .and_then(|v| v.seq())
        .and_then(|s| s.first())
        .and_then(|v| v.as_i64())
        .or_else(|| stack_id.and_then(|v| v.as_i64()));
    let key = stack_id
        .and_then(|v| v.seq())
        .and_then(|s| s.get(1))
        .map(text_of)
        .unwrap_or_default();
    let trace = call.args.get(1).map(text_of).unwrap_or_default();
    let mode = call.args.get(2).map(text_of).unwrap_or_else(|| "Unknown".to_string());

    let Some(hash) = hash else {
        warn!("client stack trace with no usable hash, mode {mode}:\n{trace}");
        return beans_answer(0);
    };

    let (error_id, first_time) = with_beans(|beans| {
        let next = beans.len() as i64 + 1;
        match beans.get_mut(&hash) {
            Some(bean) => {
                bean.occurrences += 1;
                (bean.error_id, false)
            }
            None => {
                beans.insert(hash, Bean { error_id: next, key, occurrences: 1 });
                (next, true)
            }
        }
    });

    if first_time {
        let who = match (ctx.session.user_id, ctx.session.char_id) {
            (Some(u), Some(c)) => format!("user {u}, char {c}"),
            (Some(u), None) => format!("user {u}"),
            _ => "unauthenticated".to_string(),
        };
        warn!("client {mode} #{error_id} (stack {hash}, {who}):\n{trace}");
    }
    beans_answer(error_id)
}

fn bean_delivery(call: &Call) -> Value {
    let Some(Value::Dict(pairs)) = call.args.first() else {
        return Value::None;
    };
    let mut report = Vec::new();
    with_beans(|beans| {
        for (error_id, counted) in pairs {
            let Some(error_id) = error_id.as_i64() else { continue };
            let count = counted.seq().and_then(|s| s.first()).and_then(|v| v.as_i64()).unwrap_or(0);
            if let Some(bean) = beans.values_mut().find(|b| b.error_id == error_id) {
                bean.occurrences += count;
                report.push(format!("#{error_id} x{count} ({})", first_line(&bean.key)));
            } else {
                report.push(format!("#{error_id} x{count}"));
            }
        }
    });
    if !report.is_empty() {
        debug!("bean delivery: {}", report.join(", "));
    }
    Value::None
}

fn text_of(v: &Value) -> String {
    match v {
        Value::Bytes(b) | Value::Buffer(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Unicode(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn first_line(key: &str) -> &str {
    let line = key.lines().next().unwrap_or("");
    match line.char_indices().nth(120) {
        Some((end, _)) => &line[..end],
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_survives_a_multibyte_key() {
        let key = format!("x{}", "\u{FFFD}".repeat(200));
        assert!(!key.is_char_boundary(120), "the case under test must be misaligned");
        assert_eq!(first_line(&key).chars().count(), 120);

        assert_eq!(first_line("short"), "short");
        assert_eq!(first_line(""), "");
        let exact = "y".repeat(120);
        assert_eq!(first_line(&exact), exact);
    }

    #[test]
    fn a_negative_stack_hash_round_trips() {
        let hash = -1234567890i64;
        assert_eq!(Value::Int(hash).as_i64(), Some(hash));
    }
}
