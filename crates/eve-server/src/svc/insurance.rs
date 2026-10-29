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
        // InsureShip(shipID, amount, isCorp): the quote picked (base_insurance.py
        // GetQuoteForShip: 5% of the hull's base price buys 50% cover, each
        // further 5% another 10%).
        "InsureShip" => {
            let is_corp = call.arg_i64(2).unwrap_or(0) != 0;
            let amount = call.args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
            return Some(insure(ctx, call.arg_id(0), amount, is_corp).await);
        }
        "UnInsureShip" => {
            let owner = ctx.session.char_id.unwrap_or(0) as i32;
            let sql = r#"DELETE FROM "insuranceContracts" WHERE "shipID" = $1 AND "ownerID" = $2"#;
            ctx.execute(sql, &[&(call.arg_id(0) as i32), &owner]).await;
            Value::None
        }
        _ => return None,
    };
    Some(Outcome::Value(value))
}

const REF_INSURANCE: i32 = 19;
const CONTRACT_WEEKS: i64 = 12;

/// The cover a premium buys, as a fraction of the base price.
fn cover(amount: f64, base_price: f64) -> Option<f64> {
    let step = base_price * 0.05;
    if step <= 0.0 {
        return None;
    }
    let n = (amount / step).round();
    (1.0..=6.0).contains(&n).then(|| 0.4 + 0.1 * n)
}

async fn insure(ctx: &Ctx<'_>, ship_id: i64, amount: f64, is_corp: bool) -> Outcome {
    use crate::svc::wallet::{move_cash, Payment, Transfer};
    let (Some(char_id), Some(station)) = (ctx.session.char_id, ctx.session.station_id) else {
        return Outcome::custom("Ships can only be insured in a station.");
    };
    let owner = if is_corp { ctx.session.corp_id.unwrap_or(0) } else { char_id };
    let sql = r#"SELECT t."basePrice", e."ownerID", e."locationID",
                        (SELECT count(*) FROM "insuranceContracts" c WHERE c."shipID" = e."itemID")
                   FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID"
                   JOIN "invGroups" g ON g."groupID" = t."groupID"
                  WHERE e."itemID" = $1 AND g."categoryID" = 6 AND e."singleton" = 1"#;
    let Some(r) = ctx.query_one(sql, &[&(ship_id as i32)]).await else {
        return insure_failed("That is not an assembled ship.");
    };
    let (base_price, ship_owner, location, contracts): (f64, i32, i32, i64) = (r.get(0), r.get(1), r.get(2), r.get(3));
    if ship_owner as i64 != owner || location as i64 != station {
        return insure_failed("The ship must be yours and in this station.");
    }
    if contracts > 0 {
        return Outcome::UserError { msg: "InsureShipFailedMultipleContracts".into(), args: Vec::new() };
    }
    let Some(fraction) = cover(amount, base_price) else {
        return insure_failed("That is not a premium on offer.");
    };
    let arg = ship_id.to_string();
    let payment = Payment { payer: owner, payee: None, amount, ref_type: REF_INSURANCE, arg_id1: &arg, reason: "" };
    match move_cash(ctx.db_url, payment).await {
        Ok(()) => {}
        Err(Transfer::NotEnough { need, have }) => return Outcome::not_enough_money(need, have),
        Err(Transfer::NoSuchOwner) => return insure_failed("There is no wallet to pay from."),
        Err(Transfer::Db(e)) => {
            tracing::error!("insuring {ship_id}: {}", crate::logging::chain(&e));
            return insure_failed("The insurance could not be paid for.");
        }
    }
    let now = crate::bulkdata::filetime_now();
    let end = now + CONTRACT_WEEKS * 7 * 24 * 3_600 * 10_000_000;
    let sql = r#"INSERT INTO "insuranceContracts"
                    ("contractID","shipID","ownerID","startDate","endDate","fraction","amount","payout","isCorp")
                 SELECT COALESCE(MAX("contractID"), 0) + 1, $1, $2, $3, $4, $5, $6, $7, $8 FROM "insuranceContracts""#;
    let payout = fraction * base_price;
    ctx.execute(
        sql,
        &[&(ship_id as i32), &(owner as i32), &now, &end, &fraction, &amount, &payout, &i16::from(is_corp)],
    )
    .await;
    tracing::info!("ship {ship_id} insured for {payout} ISK ({:.0}%) for {amount} ISK", fraction * 100.0);
    Outcome::Value(Value::None)
}

fn insure_failed(reason: &str) -> Outcome {
    Outcome::UserError { msg: "InsureShipFailed".into(), args: vec![("reason".into(), Value::text(reason))] }
}

#[cfg(test)]
mod tests {
    use super::cover;

    #[test]
    fn premiums_buy_cover_in_tenths() {
        assert_eq!(cover(500.0, 10_000.0), Some(0.5));
        assert!((cover(3_000.0, 10_000.0).unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(cover(100.0, 10_000.0), None);
        assert_eq!(cover(500.0, 0.0), None);
    }
}
