//! `gangsta`. There are no gangs yet: forming one needs invitations delivered to
//! another player's connection, which the server cannot do. A character is
//! always gangless.

use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};

pub async fn gangsta(call: &Call, _ctx: &Ctx<'_>) -> Option<Outcome> {
    match call.method.as_str() {
        // gang.__Initialize: (gangID, leaderID, [(charID, state)], invitations).
        "GetGangInfoForChar" => Some(Outcome::Value(Value::Tuple(vec![
            Value::None,
            Value::None,
            Value::List(Vec::new()),
            Value::List(Vec::new()),
        ]))),
        _ => None,
    }
}
