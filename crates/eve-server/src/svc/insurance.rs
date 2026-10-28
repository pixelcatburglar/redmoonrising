//! `insuranceSvc`, unbound and bound per station (base_insurance.py). Quotes are
//! computed by the client; the server keeps the contracts.

use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{line, row};

const HEADER: &[&str] = &[
    "contractID", "shipID", "ownerID", "startDate", "endDate", "fraction", "amount", "payout",
    "isCorp",
];
const SELECT: &str = r#"SELECT "contractID","shipID","ownerID","startDate","endDate","fraction",
                               "amount","payout","isCorp" FROM "insuranceContracts""#;

pub async fn insurance_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        // GetContracts(isCorp=0): the caller's contracts, or its corporation's.
        "GetContracts" => {
            let owner = if call.arg_i64(0).unwrap_or(0) != 0 {
                ctx.session.corp_id
            } else {
                ctx.session.char_id
            };
            let Some(owner) = owner else { return Some(Outcome::Value(Value::List(Vec::new()))) };
            let sql = format!(r#"{SELECT} WHERE "ownerID" = $1 ORDER BY "contractID""#);
            ctx.rowset(HEADER, &sql, &[&(owner as i32)]).await
        }
        // infosvc reads contract.ownerID/.fraction/.startDate/.endDate, or None.
        "GetContractForShip" => {
            let sql = format!(r#"{SELECT} WHERE "shipID" = $1"#);
            match ctx.query_one(&sql, &[&(call.arg_id(0) as i32)]).await {
                Some(r) => row(HEADER, line(&r)),
                None => Value::None,
            }
        }
        _ => return None,
    };
    Some(Outcome::Value(value))
}
