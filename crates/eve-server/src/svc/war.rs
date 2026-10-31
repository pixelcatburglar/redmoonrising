//! Wars between player corporations and alliances (`crpWars`).
//!
//! An alliance's executor declares war, paying CONCORD up front for the first
//! week; after that the declarer is billed weekly ([`bill_wars`]), and an
//! unpaid bill ends the war. A war turns hostile a day after it is declared
//! and stops a day after it is retracted (cfg.IsWarInHostileState). The
//! defender may make it mutual, which stops the bills. A corporation joining
//! an alliance hands its wars to the alliance.
//!
//! CONCORD (concord.rs) runs inside the world lock and cannot ask the
//! database, so the wars in force are kept in memory ([`at_war`]), reloaded
//! by the heartbeat and after every change.

use std::sync::RwLock;

use eve_marshal::Value;
use tokio_postgres::GenericClient;
use tracing::{error, info};

use crate::ballpark::Pilot;
use crate::services::{Call, Ctx, Outcome};
use crate::svc::bound;
use crate::svc::membership::{db_failed, message, transfer_refused, Refused};
use crate::svc::rows::{change, index_rowset, int, line};
use crate::svc::wallet::{self, Entry};
use crate::world::World;

const WAR: &str = "warRegistry";

pub const DAY: i64 = 24 * 60 * 60 * 10_000_000;
pub const WEEK: i64 = 7 * DAY;
/// What declaring (and each week of) a war costs, before it grows with the
/// victim's other wars.
const WAR_BASE_COST: f64 = 2_000_000.0;
/// market_refTypes / eveConstants refWarFee.
const REF_WAR_FEE: i32 = 49;
/// billTypes: War Bill.
pub const BILL_TYPE_WAR: i32 = 4;
/// eveConstants ownerCONCORD: who war and alliance bills are owed to.
pub const OWNER_CONCORD: i64 = 1_000_125;

const WAR_HEADER: &[&str] = &[
    "warID", "declaredByID", "againstID", "timeDeclared", "timeFinished", "retracted", "mutual",
];
const WAR_SELECT: &str = r#"
    SELECT "warID", "declaredByID", "againstID", "timeDeclared", "timeFinished", "retracted",
           "mutual"
      FROM "crpWars""#;

/// warRegistry, bound as (allianceOrCorpID, isMaster).
pub async fn war_registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let owner = bound::parse(&call.service).and_then(|b| b.params.first().copied());
    let asked = call.arg_i64(0).or(owner).or(ctx.session.corp_id).unwrap_or(0);
    Some(match call.method.as_str() {
        "GetWars" => {
            let sql = format!(r#"{WAR_SELECT} WHERE "declaredByID" = $1 OR "againstID" = $1 ORDER BY "warID""#);
            let items = ctx
                .query(&sql, &[&(asked as i32)])
                .await
                .iter()
                .map(|r| (Value::Int(int(r, 0).unwrap_or(0)), Value::List(line(r))))
                .collect();
            Outcome::Value(index_rowset(WAR_HEADER, "warID", items))
        }
        "GetCostOfStartingAWarAgainst" => match cost_against(ctx.db, asked).await {
            Ok(cost) => Outcome::Value(Value::Double(cost)),
            Err(refusal) => refusal,
        },
        method => Outcome::Unknown(format!("{WAR}::{method}")),
    })
}

/// corpRegistry RetractWar(warID) and ChangeMutualWarFlag(warID, mutual),
/// for a corporation outside any alliance; its CEO decides.
pub async fn corp_registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if !matches!(call.method.as_str(), "RetractWar" | "ChangeMutualWarFlag") {
        return None;
    }
    let result = async {
        let corp_id = ctx.session.corp_id.unwrap_or(0);
        let ceo = ctx
            .query_one(r#"SELECT "ceoID" FROM "corporation" WHERE "corporationID" = $1"#, &[&(corp_id as i32)])
            .await
            .and_then(|r| int(&r, 0));
        if ctx.session.alliance_id.is_some() || ceo != ctx.session.char_id {
            return Err(message("MustBeActiveCEOToEndWar"));
        }
        if call.method == "RetractWar" {
            retract(ctx, corp_id, call.arg_id(0)).await
        } else {
            set_mutual(ctx, corp_id, call.arg_id(0), call.arg_i64(1).unwrap_or(0) != 0).await
        }
    }
    .await;
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

/// The base cost, once more for every other non-mutual war in force
/// against the victim.
async fn cost_against(db: &impl GenericClient, against: i64) -> Refused<f64> {
    let wars: i64 = db
        .query_one(
            r#"SELECT count(*) FROM "crpWars"
                WHERE "againstID" = $1 AND "mutual" = 0 AND "retracted" IS NULL"#,
            &[&(against as i32)],
        )
        .await
        .map_err(|e| db_failed("counting wars", e))?
        .get(0);
    Ok(WAR_BASE_COST * (1 + wars) as f64)
}

/// `declarer` (an alliance, or a corporation outside one) declares war on
/// `against`; `payer` is the corporation whose wallet pays the first week.
/// The caller has checked that the character may act for the declarer.
pub async fn declare(ctx: &Ctx<'_>, declarer: i64, payer: i64, against: i64) -> Refused<Value> {
    if against == declarer {
        return Err(Outcome::custom("You cannot declare war on yourself."));
    }
    let target = ctx
        .query_one(
            r#"SELECT (SELECT 1 FROM alliances WHERE "allianceID" = $1 AND "deleted" = 0),
                      (SELECT "allianceID" FROM "corporation" WHERE "corporationID" = $1),
                      (SELECT 1 FROM "crpNPCCorporations" WHERE "corporationID" = $1)"#,
            &[&(against as i32)],
        )
        .await;
    let column = |i: usize| target.as_ref().and_then(|r| int(r, i));
    let (is_alliance, corp_alliance, npc) = (column(0).is_some(), column(1), column(2).is_some());
    if npc {
        return Err(message("CrpCantDeclareWarOnNPCCorp"));
    }
    if !is_alliance && corp_alliance.is_none() {
        return Err(Outcome::custom("There is no such corporation or alliance."));
    }
    if corp_alliance.is_some_and(|a| a != 0) {
        return Err(message("CrpCanNotSanctionWarAgainstOwner"));
    }

    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
    let tx = client.transaction().await.map_err(|e| db_failed("BEGIN", e))?;
    let now = crate::bulkdata::filetime_now();
    let already = tx
        .query_opt(
            r#"SELECT 1 FROM "crpWars"
                WHERE "declaredByID" = $1 AND "againstID" = $2
                  AND ("retracted" IS NULL OR "retracted" > $3)"#,
            &[&(declarer as i32), &(against as i32), &(now - DAY)],
        )
        .await
        .map_err(|e| db_failed("checking for a war", e))?;
    if already.is_some() {
        return Err(Outcome::custom("You are already at war with them."));
    }
    let cost = cost_against(&tx, against).await?;
    let entry = Entry { ref_type: REF_WAR_FEE, payer, payee: OWNER_CONCORD, arg_id1: "", reason: "" };
    wallet::post(&tx, payer, -cost, &entry).await.map_err(transfer_refused)?;
    wallet::post(&tx, OWNER_CONCORD, cost, &entry).await.map_err(transfer_refused)?;
    let war_id: i32 = tx
        .query_one(
            r#"INSERT INTO "crpWars" ("warID", "declaredByID", "againstID", "timeDeclared", "mutual")
               VALUES (nextval('dyn_id_seq'), $1, $2, $3, 0)
               RETURNING "warID""#,
            &[&(declarer as i32), &(against as i32), &now],
        )
        .await
        .map_err(|e| db_failed("recording the war", e))?
        .get(0);
    tx.commit().await.map_err(|e| db_failed("COMMIT", e))?;

    info!("{declarer} declared war on {against} (war {war_id}) for {cost} ISK");
    let new = war_line(ctx.db, war_id.into()).await?.unwrap_or_default();
    tell_war(ctx.world, ctx.db, war_id.into(), declarer, against, change(WAR_HEADER, None, Some(&new))).await;
    refresh(ctx.db).await;
    Ok(Value::None)
}

/// RetractWar(warID), by the declarer: the war stops a day later, and its
/// unpaid bill is forgiven.
pub async fn retract(ctx: &Ctx<'_>, declarer: i64, war_id: i64) -> Refused<Value> {
    let old = war_line(ctx.db, war_id).await?.ok_or_else(|| Outcome::custom("There is no such war."))?;
    if old[1].as_i64() != Some(declarer) {
        return Err(message("NotYourWarToEnd"));
    }
    if old[5] != Value::None {
        return Ok(Value::None);
    }
    let now = crate::bulkdata::filetime_now();
    ctx.db
        .execute(
            r#"UPDATE "crpWars" SET "retracted" = $2, "timeFinished" = $3 WHERE "warID" = $1"#,
            &[&(war_id as i32), &now, &(now + DAY)],
        )
        .await
        .map_err(|e| db_failed("retracting the war", e))?;
    forgive_bill(ctx.db, war_id).await;
    let mut new = old.clone();
    new[4] = Value::Long(now + DAY);
    new[5] = Value::Long(now);
    info!("{declarer} retracted war {war_id}");
    let against = old[2].as_i64().unwrap_or(0);
    tell_war(ctx.world, ctx.db, war_id, declarer, against, change(WAR_HEADER, Some(&old), Some(&new))).await;
    refresh(ctx.db).await;
    Ok(Value::None)
}

/// A war surrendered in a CEO trade (svc/pvptrade.rs): it ends as a
/// retracted one does, a day later, and its unpaid bill is forgiven.
pub async fn surrender(world: &World, db: &impl GenericClient, war_id: i64) -> Refused<()> {
    let old = war_line(db, war_id).await?.ok_or_else(|| Outcome::custom("There is no such war."))?;
    if old[5] != Value::None {
        return Err(message("CanNotSurrenderAsWarIsOver"));
    }
    let now = crate::bulkdata::filetime_now();
    db.execute(r#"UPDATE "crpWars" SET "retracted" = $2, "timeFinished" = $3 WHERE "warID" = $1"#, &[&(war_id as i32), &now, &(now + DAY)])
        .await
        .map_err(|e| db_failed("ending the war", e))?;
    forgive_bill(db, war_id).await;
    let mut new = old.clone();
    new[4] = Value::Long(now + DAY);
    new[5] = Value::Long(now);
    let (declarer, against) = (old[1].as_i64().unwrap_or(0), old[2].as_i64().unwrap_or(0));
    info!("war {war_id} of {declarer} on {against} ends in surrender");
    tell_war(world, db, war_id, declarer, against, change(WAR_HEADER, Some(&old), Some(&new))).await;
    refresh(db).await;
    Ok(())
}

/// The two sides of a war that has not ended: (declarer, against).
pub async fn sides(db: &impl GenericClient, war_id: i64) -> Option<(i64, i64)> {
    let w = war_line(db, war_id).await.ok().flatten()?;
    if w[5] != Value::None {
        return None;
    }
    Some((w[1].as_i64()?, w[2].as_i64()?))
}

/// ChangeMutualWarFlag(warID, mutual), by the defender. A mutual war is not
/// billed.
pub async fn set_mutual(ctx: &Ctx<'_>, defender: i64, war_id: i64, mutual: bool) -> Refused<Value> {
    let old = war_line(ctx.db, war_id).await?.ok_or_else(|| Outcome::custom("There is no such war."))?;
    if old[2].as_i64() != Some(defender) {
        return Err(message("NotYourWarToEnd"));
    }
    ctx.db
        .execute(r#"UPDATE "crpWars" SET "mutual" = $2 WHERE "warID" = $1"#, &[&(war_id as i32), &i16::from(mutual)])
        .await
        .map_err(|e| db_failed("changing the war", e))?;
    if mutual {
        forgive_bill(ctx.db, war_id).await;
    }
    let mut new = old.clone();
    new[6] = Value::Int(mutual.into());
    let declarer = old[1].as_i64().unwrap_or(0);
    tell_war(ctx.world, ctx.db, war_id, declarer, defender, change(WAR_HEADER, Some(&old), Some(&new))).await;
    Ok(Value::None)
}

/// A corporation joining an alliance hands it its wars, on both sides.
pub async fn adopt(db: &impl GenericClient, corp_id: i64, alliance_id: i64) -> Refused<()> {
    let now = crate::bulkdata::filetime_now();
    for column in ["declaredByID", "againstID"] {
        let sql = format!(
            r#"UPDATE "crpWars" SET "{column}" = $2
                WHERE "{column}" = $1 AND ("retracted" IS NULL OR "retracted" > $3)"#
        );
        db.execute(sql.as_str(), &[&(corp_id as i32), &(alliance_id as i32), &(now - DAY)])
            .await
            .map_err(|e| db_failed("handing the wars to the alliance", e))?;
    }
    Ok(())
}

async fn war_line(db: &impl GenericClient, war_id: i64) -> Refused<Option<Vec<Value>>> {
    let sql = format!(r#"{WAR_SELECT} WHERE "warID" = $1"#);
    db.query_opt(sql.as_str(), &[&(war_id as i32)]).await.map(|r| r.as_ref().map(line)).map_err(|e| db_failed("reading the war", e))
}

async fn forgive_bill(db: &impl GenericClient, war_id: i64) {
    let sql = r#"DELETE FROM "billsReceivable" WHERE "billTypeID" = $1 AND "externalID" = $2 AND "paid" = 0"#;
    if let Err(e) = db.execute(sql, &[&BILL_TYPE_WAR, &(war_id as i32)]).await {
        error!("forgiving the bill of war {war_id}: {}", crate::logging::chain(&e));
    }
}

/// OnWarChanged(warID, declaredByID, againstID, change), to everyone on
/// either side.
async fn tell_war(world: &World, db: &impl GenericClient, war_id: i64, declarer: i64, against: i64, change: Value) {
    let mut corps = corps_of(db, declarer).await;
    corps.extend(corps_of(db, against).await);
    let w = world.lock();
    let clients: Vec<i64> = corps.iter().flat_map(|c| w.corp_members(*c)).collect();
    let event = Value::Tuple(vec![Value::Int(war_id), Value::Int(declarer), Value::Int(against), change]);
    w.notify(&clients, "OnWarChanged", event);
}

/// The corporations an owner stands for: an alliance's members, or itself.
pub async fn corps_of(db: &impl GenericClient, owner: i64) -> Vec<i64> {
    let sql = r#"SELECT "corporationID" FROM "allianceMembers" WHERE "allianceID" = $1"#;
    let members: Vec<i64> = match db.query(sql, &[&(owner as i32)]).await {
        Ok(rows) => rows.iter().filter_map(|r| int(r, 0)).collect(),
        Err(e) => {
            error!("listing the members of {owner}: {}", crate::logging::chain(&e));
            Vec::new()
        }
    };
    if members.is_empty() { vec![owner] } else { members }
}

// ---- weekly bills ----------------------------------------------------------------------

/// Bills every non-mutual war in force for the week starting now, and ends
/// any whose last bill went unpaid. Run by the heartbeat.
pub async fn bill_wars(world: &World, db: &tokio_postgres::Client) {
    let now = crate::bulkdata::filetime_now();
    let sql = format!(r#"{WAR_SELECT} WHERE "retracted" IS NULL AND "mutual" = 0"#);
    let wars = match db.query(sql.as_str(), &[]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("reading the wars to bill: {}", crate::logging::chain(&e));
            return;
        }
    };
    for r in wars {
        let war = line(&r);
        let (war_id, declarer, against) = (int(&r, 0).unwrap_or(0), int(&r, 1).unwrap_or(0), int(&r, 2).unwrap_or(0));
        let declared = int(&r, 3).unwrap_or(now);
        match next_period(db, BILL_TYPE_WAR, war_id, declared + WEEK, now).await {
            Period::Overdue => {
                let ended = db
                    .execute(r#"UPDATE "crpWars" SET "retracted" = $2, "timeFinished" = $3 WHERE "warID" = $1"#, &[&(war_id as i32), &now, &(now + DAY)])
                    .await;
                if let Err(e) = ended {
                    error!("ending war {war_id}: {}", crate::logging::chain(&e));
                    continue;
                }
                info!("war {war_id} of {declarer} on {against} ends: its bill went unpaid");
                let mut new = war.clone();
                new[4] = Value::Long(now + DAY);
                new[5] = Value::Long(now);
                tell_war(world, db, war_id, declarer, against, change(WAR_HEADER, Some(&war), Some(&new))).await;
            }
            Period::Due { until } => {
                let amount = cost_against(db, against).await.unwrap_or(WAR_BASE_COST);
                if let Some(bill_id) = issue_bill(db, BILL_TYPE_WAR, declarer, amount, until, war_id).await {
                    let _ = db.execute(r#"UPDATE "crpWars" SET "billID" = $2 WHERE "warID" = $1"#, &[&(war_id as i32), &(bill_id as i32)]).await;
                    info!("war {war_id}: {declarer} billed {amount} ISK for the week");
                }
            }
            Period::Covered => {}
        }
    }
    refresh(db).await;
}

/// Where a recurring bill stands.
pub enum Period {
    /// Paid for (or not started) up to now.
    Covered,
    /// A new period starts: bill it, due at `until`.
    Due { until: i64 },
    /// The last bill's due date passed unpaid.
    Overdue,
}

/// Recurring bills of `bill_type` for `external_id`, each due at the end of
/// the period it pays for; the first period starts at `first`. A new bill is
/// issued only once the last one is paid and its period over.
pub async fn next_period(db: &impl GenericClient, bill_type: i32, external_id: i64, first: i64, now: i64) -> Period {
    let last = db
        .query_opt(
            r#"SELECT "dueDateTime", "paid" FROM "billsReceivable"
                WHERE "billTypeID" = $1 AND "externalID" = $2
                ORDER BY "billID" DESC LIMIT 1"#,
            &[&bill_type, &(external_id as i32)],
        )
        .await;
    let period = |start: i64| if now >= start { Period::Due { until: start + period_of(bill_type) } } else { Period::Covered };
    match last {
        Ok(None) => period(first),
        Ok(Some(r)) => {
            let due = int(&r, 0).unwrap_or(0);
            let paid = int(&r, 1).unwrap_or(0) != 0;
            match (paid, now > due) {
                (false, true) => Period::Overdue,
                (true, _) => period(due),
                (false, false) => Period::Covered,
            }
        }
        Err(e) => {
            error!("reading the bills of {external_id}: {}", crate::logging::chain(&e));
            Period::Covered
        }
    }
}

fn period_of(bill_type: i32) -> i64 {
    if bill_type == BILL_TYPE_WAR { WEEK } else { crate::svc::alliance::MAINTENANCE_PERIOD }
}

/// A bill to CONCORD; returns its ID.
pub async fn issue_bill(db: &impl GenericClient, bill_type: i32, debtor: i64, amount: f64, due: i64, external_id: i64) -> Option<i64> {
    let sql = r#"INSERT INTO "billsReceivable" ("billTypeID", "debtorID", "creditorID", "amount", "dueDateTime", "externalID")
                 VALUES ($1, $2, $3, $4, $5, $6) RETURNING "billID""#;
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 6] =
        [&bill_type, &(debtor as i32), &(OWNER_CONCORD as i32), &amount, &due, &(external_id as i32)];
    match db.query_one(sql, &params).await {
        Ok(r) => int(&r, 0),
        Err(e) => {
            error!("billing {debtor}: {}", crate::logging::chain(&e));
            None
        }
    }
}

// ---- hostility, for CONCORD -----------------------------------------------------------

/// A war in force: (declarer, against, declared, retracted), FILETIME.
type Span = (i64, i64, i64, Option<i64>);

static IN_FORCE: RwLock<Vec<Span>> = RwLock::new(Vec::new());

/// Reloads the wars in force.
pub async fn refresh(db: &impl GenericClient) {
    let since = crate::bulkdata::filetime_now() - DAY;
    let sql = r#"SELECT "declaredByID", "againstID", "timeDeclared", "retracted" FROM "crpWars"
                  WHERE "retracted" IS NULL OR "retracted" > $1"#;
    match db.query(sql, &[&since]).await {
        Ok(rows) => {
            let spans = rows
                .iter()
                .map(|r| (int(r, 0).unwrap_or(0), int(r, 1).unwrap_or(0), int(r, 2).unwrap_or(0), int(r, 3)))
                .collect();
            if let Ok(mut w) = IN_FORCE.write() {
                *w = spans;
            }
        }
        Err(e) => error!("reading the wars in force: {}", crate::logging::chain(&e)),
    }
}

/// Whether two pilots' sides are at war in a hostile state, where each side
/// is the pilot's alliance, or corporation outside one.
pub fn at_war(a: &Pilot, b: &Pilot) -> bool {
    sides_at_war(side(a), side(b))
}

/// A pilot's side in a war: its alliance, or its corporation outside one.
pub fn side(p: &Pilot) -> i64 {
    p.alliance_id.filter(|a| *a != 0).unwrap_or(p.corp_id)
}

/// Puts a war between two sides in force, a few days old, for tests.
#[cfg(test)]
pub fn in_force_for_test(a: i64, b: i64) {
    let declared = crate::bulkdata::filetime_now() - 3 * DAY;
    if let Ok(mut w) = IN_FORCE.write() {
        w.push((a, b, declared, None));
    }
}

/// Whether two sides (alliances, or corporations outside one) are at war in
/// a hostile state.
pub fn sides_at_war(x: i64, y: i64) -> bool {
    let now = crate::bulkdata::filetime_now();
    IN_FORCE.read().is_ok_and(|wars| wars.iter().any(|w| hostile(*w, x, y, now)))
}

/// The sides `x` is at war with, in a hostile state.
pub fn enemies_of(x: i64) -> Vec<i64> {
    let now = crate::bulkdata::filetime_now();
    let Ok(wars) = IN_FORCE.read() else { return Vec::new() };
    let mut enemies: Vec<i64> = wars
        .iter()
        .filter_map(|w| {
            let y = if w.0 == x { w.1 } else if w.1 == x { w.0 } else { return None };
            hostile(*w, x, y, now).then_some(y)
        })
        .collect();
    enemies.sort_unstable();
    enemies.dedup();
    enemies
}

fn hostile((by, against, declared, retracted): Span, x: i64, y: i64, now: i64) -> bool {
    let pair = (by == x && against == y) || (by == y && against == x);
    pair && now - declared >= DAY && retracted.is_none_or(|r| now - r < DAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_war_is_hostile_from_a_day_after_declaration_to_a_day_after_retraction() {
        let declared = 100 * DAY;
        let war = (1, 2, declared, None);
        assert!(!hostile(war, 1, 2, declared + DAY - 1));
        assert!(hostile(war, 2, 1, declared + DAY));
        assert!(!hostile(war, 1, 3, declared + 2 * DAY));
        let retracted = (1, 2, declared, Some(declared + 3 * DAY));
        assert!(hostile(retracted, 1, 2, declared + 4 * DAY - 1));
        assert!(!hostile(retracted, 1, 2, declared + 4 * DAY));
    }
}
