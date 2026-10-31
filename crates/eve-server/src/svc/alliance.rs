//! Alliances: the registry clients bind per alliance (`allianceRegistry`),
//! the corporation's side of founding and joining one (`corpRegistry`), and
//! wars.
//!
//! A corporation's CEO founds an alliance or applies to one; the executor
//! corporation's directors accept or reject; an accepted application takes
//! effect a day later ([`take_effect`], from the heartbeat). Member
//! corporations each back one member as executor, and the most backed one
//! leads. A corporation leaves by quitting or by being kicked out.
//!
//! The executor's directors also set the alliance's relationships and run
//! its wars (svc/war.rs). Every fortnight CONCORD bills the alliance for its
//! member corporations ([`bill_alliances`]); the executor's accountants pay
//! alliance bills from the executor corporation's wallet.

use eve_marshal::Value;
use tokio_postgres::{Client, GenericClient};
use tracing::{error, info};

use crate::services::{Call, Ctx, Outcome};
use crate::session::{CORP_ROLE_ACCOUNTANT, CORP_ROLE_DIRECTOR};
use crate::svc::bound;
use crate::svc::membership::{access_denied, db_failed, exists, free_tickers, message, name_error, ticker_ok, transfer_refused, Refused};
use crate::svc::rows::{change, index_rowset, int, line, rowset};
use crate::svc::wallet::{self, Entry, Side};
use crate::svc::war;
use crate::world::World;

const ALLIANCE: &str = "allianceRegistry";

/// eveConstants allianceApplication*.
const APPLICATION_NEW: i16 = 1;
const APPLICATION_ACCEPTED: i16 = 2;
const APPLICATION_EFFECTIVE: i16 = 3;
const APPLICATION_REJECTED: i16 = 4;
/// eveConstants allianceCreationCost, paid by the founding corporation.
const CREATION_COST: f64 = 1_000_000_000.0;
/// market_refTypes: Alliance Registration Fee.
const REF_ALLIANCE_REGISTRATION_FEE: i32 = 48;
const TICKS_PER_SECOND: i64 = 10_000_000;
/// eveConstants allianceMembershipCost: owed per member corporation each
/// period.
const MEMBERSHIP_COST: f64 = 2_000_000.0;
pub const MAINTENANCE_PERIOD: i64 = 14 * war::DAY;
/// billTypes: Alliance Maintainance Bill.
const BILL_TYPE_ALLIANCE_MAINTENANCE: i32 = 5;
/// eveConstants allianceRelationship*: NAP, Friend, Competitor, Enemy.
const RELATIONSHIPS: std::ops::RangeInclusive<i64> = 1..=4;

const ALLIANCE_HEADER: &[&str] = &[
    "allianceID", "allianceName", "shortName", "description", "url", "executorCorpID",
    "creatorCorpID", "creatorCharID", "dictatorial", "startDate", "memberCount", "deleted",
];
const ALLIANCE_SELECT: &str = r#"
    SELECT "allianceID", "allianceName", "shortName", "description", "url", "executorCorpID",
           "creatorCorpID", "creatorCharID", "dictatorial", "startDate", "memberCount", "deleted"
      FROM alliances"#;

const MEMBER_HEADER: &[&str] = &["allianceID", "corporationID", "chosenExecutorID", "startDate"];
const MEMBER_SELECT: &str =
    r#"SELECT "allianceID", "corporationID", "chosenExecutorID", "startDate" FROM "allianceMembers""#;

const APPLICATION_HEADER: &[&str] =
    &["allianceID", "corporationID", "applicationText", "applicationDateTime", "state"];
const APPLICATION_SELECT: &str = r#"
    SELECT "allianceID", "corporationID", "applicationText", "applicationDateTime", "state"
      FROM "allianceApplications""#;

const RELATIONSHIP_HEADER: &[&str] = &["fromID", "toID", "relationship"];
const RELATIONSHIP_SELECT: &str =
    r#"SELECT "fromID", "toID", "relationship" FROM "allianceRelationships""#;

// ---- allianceRegistry ------------------------------------------------------------------

pub async fn alliance_registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    // Bound as (allianceID, isMaster); unbound calls name the alliance themselves.
    let subject = bound::parse(&call.service).and_then(|b| b.params.first().copied()).or(ctx.session.alliance_id);
    let text_arg = |i: usize| call.arg_text(i).unwrap_or_default();
    let result = match call.method.as_str() {
        "GetAlliance" => {
            let id = call.arg_i64(0).or(subject).filter(|id| *id != 0);
            let Some(id) = id else {
                return Some(Outcome::Unknown("allianceRegistry::GetAlliance with no alliance".into()));
            };
            alliance_row(ctx, id).await.ok_or_else(|| message("AllianceNotFound"))
        }
        "GetRankedAlliances" => {
            let sql = format!(r#"{ALLIANCE_SELECT} WHERE "deleted" = 0 ORDER BY "memberCount" DESC, "allianceID""#);
            Ok(ctx.rowset(ALLIANCE_HEADER, &sql, &[]).await)
        }
        "GetMembers" => Ok(keyed(ctx, MEMBER_HEADER, MEMBER_SELECT, "allianceID", subject.unwrap_or(0), 1).await),
        "GetApplications" => {
            let sql = format!(r#"{APPLICATION_SELECT} WHERE "deleted" = 0 AND "allianceID" = $1"#);
            Ok(keyed_rows(ctx, APPLICATION_HEADER, &sql, subject.unwrap_or(0), 1).await)
        }
        "UpdateApplication" => {
            let state = call.arg_i64(2).unwrap_or(-1) as i16;
            answer_application(ctx, subject.unwrap_or(0), call.arg_id(0), &text_arg(1), state).await
        }
        "DeclareExecutorSupport" => declare_support(ctx, subject.unwrap_or(0), call.arg_id(0)).await,
        "DeleteMember" => delete_member(ctx, subject.unwrap_or(0), call.arg_id(0)).await,
        "UpdateAlliance" => update_alliance(ctx, subject.unwrap_or(0), &text_arg(0), &text_arg(1)).await,
        "GetRelationships" => Ok(keyed(ctx, RELATIONSHIP_HEADER, RELATIONSHIP_SELECT, "fromID", subject.unwrap_or(0), 1).await),
        "SetRelationship" => set_relationship(ctx, subject.unwrap_or(0), call.arg_i64(0).unwrap_or(0), call.arg_id(1)).await,
        "DeleteRelationship" => delete_relationship(ctx, subject.unwrap_or(0), call.arg_id(0)).await,
        "DeclareWarAgainst" => declare_war(ctx, subject.unwrap_or(0), call.arg_id(0)).await,
        "RetractWar" => retract_war(ctx, subject.unwrap_or(0), call.arg_id(0)).await,
        "ChangeMutualWarFlag" => {
            mutual_war(ctx, subject.unwrap_or(0), call.arg_id(0), call.arg_i64(1).unwrap_or(0) != 0).await
        }
        "GetBills" => Ok(wallet::bills(ctx, subject, Side::Payable).await),
        "GetBillsReceivable" => Ok(wallet::bills(ctx, subject, Side::Receivable).await),
        "GetBillBalance" => bill_balance(ctx, subject.unwrap_or(0), call.arg_id(0)).await,
        "PayBill" => pay_bill(ctx, subject.unwrap_or(0), call.arg_id(0)).await,
        method => return Some(Outcome::Unknown(format!("{ALLIANCE}::{method}"))),
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

async fn alliance_row(ctx: &Ctx<'_>, alliance_id: i64) -> Option<Value> {
    let sql = format!(r#"{ALLIANCE_SELECT} WHERE "allianceID" = $1"#);
    ctx.query_one(&sql, &[&(alliance_id as i32)]).await.map(|r| crate::svc::rows::row(ALLIANCE_HEADER, line(&r)))
}

/// UpdateApplication(corporationID, text, state): the executor's directors
/// accept or reject. An acceptance starts the clock on the membership.
async fn answer_application(ctx: &Ctx<'_>, alliance_id: i64, corp_id: i64, note: &str, state: i16) -> Refused<Value> {
    require_executor_director(ctx, alliance_id).await?;
    let Some(old) = application(ctx.db, alliance_id, corp_id).await? else {
        return Err(Outcome::custom("That application no longer exists."));
    };
    if application_state(&old) != APPLICATION_NEW || ![APPLICATION_ACCEPTED, APPLICATION_REJECTED].contains(&state) {
        return Err(Outcome::custom("The application cannot be moved to that state now."));
    }
    let when = if state == APPLICATION_ACCEPTED { Value::Long(crate::bulkdata::filetime_now()) } else { old[3].clone() };
    ctx.db
        .execute(
            r#"UPDATE "allianceApplications" SET "applicationText" = $3, "state" = $4, "applicationDateTime" = $5
                WHERE "allianceID" = $1 AND "corporationID" = $2"#,
            &[&(alliance_id as i32), &(corp_id as i32), &note, &state, &when.as_i64().unwrap_or(0)],
        )
        .await
        .map_err(|e| db_failed("answering the application", e))?;
    let new = vec![old[0].clone(), old[1].clone(), Value::text(note), when, Value::Int(state.into())];
    info!("alliance {alliance_id} answered corporation {corp_id}'s application with state {state}");
    let corps = with(member_corps(ctx.db, alliance_id).await?, corp_id);
    tell(ctx.world, &corps, "OnAllianceApplicationChanged", application_event(alliance_id, corp_id, change(APPLICATION_HEADER, Some(&old), Some(&new))));
    Ok(Value::None)
}

/// DeclareExecutorSupport(corporationID): the caller's corporation backs
/// another member to lead. The most backed member leads; a tie keeps the
/// executor there is.
async fn declare_support(ctx: &Ctx<'_>, alliance_id: i64, choice: i64) -> Refused<Value> {
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    if ctx.session.alliance_id != Some(alliance_id) || !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err(access_denied("only directors of a member corporation may back an executor"));
    }
    let members = member_corps(ctx.db, alliance_id).await?;
    if !members.contains(&choice) {
        return Err(Outcome::custom("That corporation is not a member of the alliance."));
    }
    let alliance = alliance_line(ctx.db, alliance_id).await?.ok_or_else(|| message("AllianceNotFound"))?;
    if alliance[8].as_i64() == Some(1) && alliance[5].as_i64() != Some(choice) {
        return Err(message("CanNotChooseExecutorAsDictatorial"));
    }

    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
    let tx = client.transaction().await.map_err(|e| db_failed("BEGIN", e))?;
    let old = member_line(&tx, corp_id).await?.ok_or_else(|| Outcome::custom("Your corporation is not a member."))?;
    tx.execute(r#"UPDATE "allianceMembers" SET "chosenExecutorID" = $2 WHERE "corporationID" = $1"#, &[&(corp_id as i32), &(choice as i32)])
        .await
        .map_err(|e| db_failed("recording the choice", e))?;
    let new = member_line(&tx, corp_id).await?.unwrap_or_default();
    let elected = elect(&tx, alliance_id).await?;
    tx.commit().await.map_err(|e| db_failed("COMMIT", e))?;

    info!("corporation {corp_id} backs {choice} to lead alliance {alliance_id}");
    tell(ctx.world, &members, "OnAllianceMemberChanged", member_event(alliance_id, corp_id, change(MEMBER_HEADER, Some(&old), Some(&new))));
    if let Some((was, now)) = elected {
        info!("corporation {now} now leads alliance {alliance_id} (was {was})");
        tell_executor(ctx.world, &members, alliance_id, was, now);
    }
    Ok(Value::None)
}

/// DeleteMember(corporationID): a member's directors take it out, or the
/// executor's directors kick it out. The executor itself cannot leave while
/// others remain; the last member leaving closes the alliance.
async fn delete_member(ctx: &Ctx<'_>, alliance_id: i64, corp_id: i64) -> Refused<Value> {
    let own_corp = ctx.session.corp_id == Some(corp_id);
    if ctx.session.alliance_id != Some(alliance_id) || !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err(access_denied("only directors may take a corporation out of the alliance"));
    }
    let executor = executor_of(ctx.db, alliance_id).await?;
    if !own_corp && ctx.session.corp_id != executor {
        return Err(access_denied("only the executor corporation may kick members out"));
    }
    let members = member_corps(ctx.db, alliance_id).await?;
    if !members.contains(&corp_id) {
        return Err(Outcome::custom("That corporation is not a member of the alliance."));
    }
    if executor == Some(corp_id) && members.len() > 1 {
        return Err(Outcome::custom(
            "The executor corporation cannot leave while other corporations remain. Back another member as executor first.",
        ));
    }

    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
    let tx = client.transaction().await.map_err(|e| db_failed("BEGIN", e))?;
    let old = member_line(&tx, corp_id).await?.unwrap_or_default();
    for sql in [
        r#"DELETE FROM "allianceMembers" WHERE "corporationID" = $2 AND "allianceID" = $1"#,
        r#"DELETE FROM "allianceApplications" WHERE "corporationID" = $2 AND "allianceID" = $1"#,
        r#"UPDATE "allianceMembers" SET "chosenExecutorID" = NULL WHERE "allianceID" = $1 AND "chosenExecutorID" = $2"#,
        r#"UPDATE "corporation" SET "allianceID" = 0 WHERE "corporationID" = $2 AND "allianceID" = $1"#,
    ] {
        tx.execute(sql, &[&(alliance_id as i32), &(corp_id as i32)]).await.map_err(|e| db_failed("leaving the alliance", e))?;
    }
    tx.execute(
        r#"UPDATE alliances SET "memberCount" = GREATEST(0, "memberCount" - 1),
                                "deleted" = CASE WHEN "memberCount" <= 1 THEN 1 ELSE "deleted" END
            WHERE "allianceID" = $1"#,
        &[&(alliance_id as i32)],
    )
    .await
    .map_err(|e| db_failed("counting members", e))?;
    tx.commit().await.map_err(|e| db_failed("COMMIT", e))?;

    if members.len() == 1 {
        info!("corporation {corp_id} left alliance {alliance_id}, which is closed");
    } else if own_corp {
        info!("corporation {corp_id} quit alliance {alliance_id}");
    } else {
        info!("corporation {corp_id} was kicked out of alliance {alliance_id}");
    }
    tell(ctx.world, &members, "OnAllianceMemberChanged", member_event(alliance_id, corp_id, change(MEMBER_HEADER, Some(&old), None)));
    tell_corporation_alliance(ctx.world, corp_id, Some(alliance_id), None);
    Ok(Value::None)
}

/// UpdateAlliance(description, url), by the executor's directors.
async fn update_alliance(ctx: &Ctx<'_>, alliance_id: i64, description: &str, url: &str) -> Refused<Value> {
    require_executor_director(ctx, alliance_id).await?;
    let old = alliance_line(ctx.db, alliance_id).await?.ok_or_else(|| message("AllianceNotFound"))?;
    ctx.db
        .execute(r#"UPDATE alliances SET "description" = $2, "url" = $3 WHERE "allianceID" = $1"#, &[&(alliance_id as i32), &description, &url])
        .await
        .map_err(|e| db_failed("updating the alliance", e))?;
    let mut new = old.clone();
    new[3] = Value::text(description);
    new[4] = Value::text(url);
    let members = member_corps(ctx.db, alliance_id).await?;
    let event = Value::Tuple(vec![Value::Int(alliance_id), change(ALLIANCE_HEADER, Some(&old), Some(&new))]);
    tell(ctx.world, &members, "OnAllianceChanged", event);
    Ok(Value::None)
}

// ---- corpRegistry: the corporation's side ----------------------------------------------

pub async fn corp_registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let text_arg = |i: usize| call.arg_text(i).unwrap_or_default();
    let result = match call.method.as_str() {
        "GetSuggestedAllianceShortNames" => {
            let names = free_tickers(ctx, r#"SELECT "shortName" FROM alliances"#, &text_arg(0)).await;
            Ok(rowset(&["shortName"], names.into_iter().map(|t| Value::List(vec![Value::text(t)])).collect()))
        }
        "CreateAlliance" => create_alliance(ctx, &text_arg(0), &text_arg(1), &text_arg(2), &text_arg(3)).await,
        "ApplyToJoinAlliance" => apply(ctx, call.arg_id(0), &text_arg(1)).await,
        "GetAllianceApplications" => {
            let sql = format!(r#"{APPLICATION_SELECT} WHERE "deleted" = 0 AND "corporationID" = $1"#);
            Ok(keyed_rows(ctx, APPLICATION_HEADER, &sql, ctx.session.corp_id.unwrap_or(0), 0).await)
        }
        "DeleteAllianceApplication" => withdraw(ctx, call.arg_id(0)).await,
        _ => return None,
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

/// CreateAlliance(name, shortName, description, url): the CEO's corporation
/// founds it, pays for it from its wallet, and is its first executor.
async fn create_alliance(ctx: &Ctx<'_>, name: &str, short_name: &str, description: &str, url: &str) -> Refused<Value> {
    let (name, short_name) = (name.trim(), short_name.trim());
    let char_id = ctx.session.char_id.unwrap_or(0);
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    let corp = corp_line(ctx.db, corp_id).await?;
    if corp.ceo != char_id {
        return Err(access_denied("only the CEO can found an alliance"));
    }
    if corp.alliance_id != 0 {
        return Err(message("AllianceCreateFailCorpInAlliance"));
    }
    if let Some(key) = name_error("Alliance", name) {
        return Err(message(&key));
    }
    if !ticker_ok(short_name) {
        return Err(message("AllianceShortNameInvalid"));
    }

    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
    let tx = client.transaction().await.map_err(|e| db_failed("BEGIN", e))?;
    if exists(&tx, r#"SELECT 1 FROM alliances WHERE lower("allianceName") = lower($1)"#, name).await? {
        return Err(message("AllianceNameInvalidTaken"));
    }
    if exists(&tx, r#"SELECT 1 FROM alliances WHERE lower("shortName") = lower($1)"#, short_name).await? {
        return Err(message("AllianceShortNameInvalidTaken"));
    }
    let entry = Entry { ref_type: REF_ALLIANCE_REGISTRATION_FEE, payer: corp_id, payee: 0, arg_id1: "", reason: "" };
    wallet::post(&tx, corp_id, -CREATION_COST, &entry).await.map_err(transfer_refused)?;

    let now = crate::bulkdata::filetime_now();
    let corp32 = corp_id as i32;
    let alliance_id: i32 = tx
        .query_one(
            r#"INSERT INTO alliances
                   ("allianceName", "shortName", "description", "url", "executorCorpID",
                    "creatorCorpID", "creatorCharID", "startDate", "memberCount")
               VALUES ($1, $2, $3, $4, $5, $5, $6, $7, 1)
               RETURNING "allianceID""#,
            &[&name, &short_name, &description, &url, &corp32, &(char_id as i32), &now],
        )
        .await
        .map_err(|e| db_failed("inserting the alliance", e))?
        .get(0);
    // alliance_shortnames follows by trigger (sync_alliance_shortnames).
    join(&tx, alliance_id.into(), corp_id, now).await?;
    tx.execute(r#"DELETE FROM "allianceApplications" WHERE "corporationID" = $1"#, &[&corp32])
        .await
        .map_err(|e| db_failed("clearing old applications", e))?;
    tx.commit().await.map_err(|e| db_failed("COMMIT", e))?;

    info!("corporation {corp_id} founded alliance {name:?} <{short_name}> ({alliance_id})");
    tell_corporation_alliance(ctx.world, corp_id, None, Some(alliance_id.into()));
    Ok(Value::Int(alliance_id.into()))
}

/// ApplyToJoinAlliance(allianceID, text), by the CEO or a director.
async fn apply(ctx: &Ctx<'_>, alliance_id: i64, note: &str) -> Refused<Value> {
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    if !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err(access_denied("only directors may apply to join an alliance"));
    }
    if corp_line(ctx.db, corp_id).await?.alliance_id != 0 {
        return Err(message("AllianceJoinFailCorpInAlliance"));
    }
    let alliance = alliance_line(ctx.db, alliance_id).await?;
    if alliance.as_ref().is_none_or(|a| a[11].as_i64() != Some(0)) {
        return Err(message("AllianceNotFound"));
    }
    if application(ctx.db, alliance_id, corp_id).await?.is_some() {
        return Err(message("CantApplyTwiceToAlliance"));
    }
    let now = crate::bulkdata::filetime_now();
    ctx.db
        .execute(
            r#"INSERT INTO "allianceApplications"
                   ("allianceID", "corporationID", "applicationText", "applicationDateTime", "state", "deleted")
               VALUES ($1, $2, $3, $4, $5, 0)
               ON CONFLICT ("allianceID", "corporationID") DO UPDATE
                  SET "applicationText" = $3, "applicationDateTime" = $4, "state" = $5, "deleted" = 0"#,
            &[&(alliance_id as i32), &(corp_id as i32), &note, &now, &APPLICATION_NEW],
        )
        .await
        .map_err(|e| db_failed("filing the application", e))?;
    let new = vec![Value::Int(alliance_id), Value::Int(corp_id), Value::text(note), Value::Long(now), Value::Int(APPLICATION_NEW.into())];
    info!("corporation {corp_id} applied to join alliance {alliance_id}");
    let corps = with(member_corps(ctx.db, alliance_id).await?, corp_id);
    tell(ctx.world, &corps, "OnAllianceApplicationChanged", application_event(alliance_id, corp_id, change(APPLICATION_HEADER, None, Some(&new))));
    Ok(Value::None)
}

/// DeleteAllianceApplication(allianceID): the corporation withdraws (or
/// clears away a rejected) application.
async fn withdraw(ctx: &Ctx<'_>, alliance_id: i64) -> Refused<Value> {
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    if !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err(access_denied("only directors may withdraw an alliance application"));
    }
    let Some(old) = application(ctx.db, alliance_id, corp_id).await? else { return Ok(Value::None) };
    if application_state(&old) == APPLICATION_EFFECTIVE {
        return Err(Outcome::custom("That application has taken effect; leave the alliance instead."));
    }
    ctx.db
        .execute(r#"DELETE FROM "allianceApplications" WHERE "allianceID" = $1 AND "corporationID" = $2"#, &[&(alliance_id as i32), &(corp_id as i32)])
        .await
        .map_err(|e| db_failed("withdrawing the application", e))?;
    let corps = with(member_corps(ctx.db, alliance_id).await?, corp_id);
    tell(ctx.world, &corps, "OnAllianceApplicationChanged", application_event(alliance_id, corp_id, change(APPLICATION_HEADER, Some(&old), None)));
    Ok(Value::None)
}

// ---- memberships taking effect ---------------------------------------------------------

/// Accepted applications older than `delay_secs` become memberships. Run by
/// the heartbeat on its own connection.
pub async fn take_effect(world: &World, db: &mut Client, delay_secs: i64) {
    let due = crate::bulkdata::filetime_now() - delay_secs * TICKS_PER_SECOND;
    let sql = format!(r#"{APPLICATION_SELECT} WHERE "deleted" = 0 AND "state" = $1 AND "applicationDateTime" <= $2"#);
    let rows = match db.query(sql.as_str(), &[&APPLICATION_ACCEPTED, &due]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("reading accepted alliance applications: {}", crate::logging::chain(&e));
            return;
        }
    };
    for r in rows {
        let old = line(&r);
        let (alliance_id, corp_id) = (int(&r, 0).unwrap_or(0), int(&r, 1).unwrap_or(0));
        match admit(db, alliance_id, corp_id).await {
            Ok(Some(member)) => {
                info!("corporation {corp_id} joined alliance {alliance_id}");
                let mut new = old.clone();
                new[4] = Value::Int(APPLICATION_EFFECTIVE.into());
                let members = member_corps(&*db, alliance_id).await.unwrap_or_default();
                tell(world, &members, "OnAllianceApplicationChanged", application_event(alliance_id, corp_id, change(APPLICATION_HEADER, Some(&old), Some(&new))));
                tell(world, &members, "OnAllianceMemberChanged", member_event(alliance_id, corp_id, change(MEMBER_HEADER, None, Some(&member))));
                tell_corporation_alliance(world, corp_id, None, Some(alliance_id));
            }
            Ok(None) => info!("alliance application of {corp_id} to {alliance_id} lapsed: the corporation or the alliance moved on"),
            Err(_) => {}
        }
    }
}

/// Makes one accepted application a membership; None (and the application
/// dropped) if the corporation joined elsewhere or the alliance closed.
async fn admit(db: &mut Client, alliance_id: i64, corp_id: i64) -> Refused<Option<Vec<Value>>> {
    let tx = db.transaction().await.map_err(|e| db_failed("BEGIN", e))?;
    let free = tx
        .query_opt(
            r#"SELECT 1 FROM "corporation" co, alliances a
                WHERE co."corporationID" = $2 AND co."allianceID" = 0
                  AND a."allianceID" = $1 AND a."deleted" = 0
                FOR UPDATE"#,
            &[&(alliance_id as i32), &(corp_id as i32)],
        )
        .await
        .map_err(|e| db_failed("checking the membership", e))?
        .is_some();
    if !free {
        tx.execute(r#"DELETE FROM "allianceApplications" WHERE "allianceID" = $1 AND "corporationID" = $2"#, &[&(alliance_id as i32), &(corp_id as i32)])
            .await
            .map_err(|e| db_failed("dropping the application", e))?;
        tx.commit().await.map_err(|e| db_failed("COMMIT", e))?;
        return Ok(None);
    }
    join(&tx, alliance_id, corp_id, crate::bulkdata::filetime_now()).await?;
    tx.execute(
        r#"UPDATE "allianceApplications" SET "state" = $3 WHERE "allianceID" = $1 AND "corporationID" = $2"#,
        &[&(alliance_id as i32), &(corp_id as i32), &APPLICATION_EFFECTIVE],
    )
    .await
    .map_err(|e| db_failed("closing the application", e))?;
    tx.execute(r#"UPDATE alliances SET "memberCount" = "memberCount" + 1 WHERE "allianceID" = $1"#, &[&(alliance_id as i32)])
        .await
        .map_err(|e| db_failed("counting members", e))?;
    let member = member_line(&tx, corp_id).await?;
    tx.commit().await.map_err(|e| db_failed("COMMIT", e))?;
    Ok(member)
}

/// The corporation becomes a member, backing the executor there is (itself,
/// for a founder), and its wars become the alliance's.
async fn join(db: &impl GenericClient, alliance_id: i64, corp_id: i64, now: i64) -> Refused<()> {
    war::adopt(db, corp_id, alliance_id).await?;
    db.execute(
        r#"INSERT INTO "allianceMembers" ("allianceID", "corporationID", "chosenExecutorID", "startDate")
           SELECT $1, $2, a."executorCorpID", $3 FROM alliances a WHERE a."allianceID" = $1"#,
        &[&(alliance_id as i32), &(corp_id as i32), &now],
    )
    .await
    .map_err(|e| db_failed("adding the member", e))?;
    db.execute(r#"UPDATE "corporation" SET "allianceID" = $1 WHERE "corporationID" = $2"#, &[&(alliance_id as i32), &(corp_id as i32)])
        .await
        .map_err(|e| db_failed("recording the alliance", e))?;
    Ok(())
}

/// Counts the members' backing and moves the lead if another member has
/// more of it than the executor; returns (old, new) when it moves.
async fn elect(db: &impl GenericClient, alliance_id: i64) -> Refused<Option<(i64, i64)>> {
    let rows = db
        .query(
            r#"SELECT m."corporationID",
                      (SELECT count(*) FROM "allianceMembers" b
                        WHERE b."allianceID" = $1 AND b."chosenExecutorID" = m."corporationID"),
                      a."executorCorpID"
                 FROM "allianceMembers" m JOIN alliances a ON a."allianceID" = m."allianceID"
                WHERE m."allianceID" = $1
                ORDER BY m."startDate", m."corporationID""#,
            &[&(alliance_id as i32)],
        )
        .await
        .map_err(|e| db_failed("counting executor support", e))?;
    let backing: Vec<(i64, i64)> = rows.iter().map(|r| (int(r, 0).unwrap_or(0), int(r, 1).unwrap_or(0))).collect();
    let Some(executor) = rows.first().and_then(|r| int(r, 2)) else { return Ok(None) };
    let Some(winner) = most_backed(&backing, executor) else { return Ok(None) };
    if winner == executor {
        return Ok(None);
    }
    db.execute(r#"UPDATE alliances SET "executorCorpID" = $2 WHERE "allianceID" = $1"#, &[&(alliance_id as i32), &(winner as i32)])
        .await
        .map_err(|e| db_failed("changing the executor", e))?;
    Ok(Some((executor, winner)))
}

/// The member with strictly the most backing, else the executor; `backing`
/// is (corporation, members backing it), oldest member first.
fn most_backed(backing: &[(i64, i64)], executor: i64) -> Option<i64> {
    let held = backing.iter().find(|(c, _)| *c == executor).map(|(_, n)| *n);
    let (_, (best, votes)) = backing.iter().copied().enumerate().max_by(|(i, a), (j, b)| a.1.cmp(&b.1).then(j.cmp(i)))?;
    match held {
        Some(n) if n >= votes => Some(executor),
        _ => Some(best),
    }
}

// ---- reads and notifications -----------------------------------------------------------

struct CorpLine {
    ceo: i64,
    alliance_id: i64,
}

async fn corp_line(db: &impl GenericClient, corp_id: i64) -> Refused<CorpLine> {
    let r = db
        .query_opt(r#"SELECT "ceoID", "allianceID" FROM "corporation" WHERE "corporationID" = $1"#, &[&(corp_id as i32)])
        .await
        .map_err(|e| db_failed("reading the corporation", e))?
        .ok_or_else(|| Outcome::custom("Your corporation could not be found."))?;
    Ok(CorpLine { ceo: int(&r, 0).unwrap_or(0), alliance_id: int(&r, 1).unwrap_or(0) })
}

async fn alliance_line(db: &impl GenericClient, alliance_id: i64) -> Refused<Option<Vec<Value>>> {
    let sql = format!(r#"{ALLIANCE_SELECT} WHERE "allianceID" = $1"#);
    db.query_opt(sql.as_str(), &[&(alliance_id as i32)]).await.map(|r| r.as_ref().map(line)).map_err(|e| db_failed("reading the alliance", e))
}

async fn member_line(db: &impl GenericClient, corp_id: i64) -> Refused<Option<Vec<Value>>> {
    let sql = format!(r#"{MEMBER_SELECT} WHERE "corporationID" = $1"#);
    db.query_opt(sql.as_str(), &[&(corp_id as i32)]).await.map(|r| r.as_ref().map(line)).map_err(|e| db_failed("reading the member", e))
}

async fn application(db: &impl GenericClient, alliance_id: i64, corp_id: i64) -> Refused<Option<Vec<Value>>> {
    let sql = format!(r#"{APPLICATION_SELECT} WHERE "deleted" = 0 AND "allianceID" = $1 AND "corporationID" = $2"#);
    db.query_opt(sql.as_str(), &[&(alliance_id as i32), &(corp_id as i32)])
        .await
        .map(|r| r.as_ref().map(line))
        .map_err(|e| db_failed("reading the application", e))
}

fn application_state(line: &[Value]) -> i16 {
    line.get(4).and_then(|v| v.as_i64()).unwrap_or(-1) as i16
}

async fn member_corps(db: &impl GenericClient, alliance_id: i64) -> Refused<Vec<i64>> {
    db.query(r#"SELECT "corporationID" FROM "allianceMembers" WHERE "allianceID" = $1"#, &[&(alliance_id as i32)])
        .await
        .map(|rows| rows.iter().filter_map(|r| int(r, 0)).collect())
        .map_err(|e| db_failed("listing the members", e))
}

async fn executor_of(db: &impl GenericClient, alliance_id: i64) -> Refused<Option<i64>> {
    Ok(alliance_line(db, alliance_id).await?.and_then(|a| a[5].as_i64()))
}

/// The executor corporation, if the caller is one of its directors.
async fn require_executor_director(ctx: &Ctx<'_>, alliance_id: i64) -> Refused<i64> {
    require_executor_role(ctx, alliance_id, CORP_ROLE_DIRECTOR, "directors").await
}

async fn require_executor_role(ctx: &Ctx<'_>, alliance_id: i64, role: i64, who: &str) -> Refused<i64> {
    let executor = executor_of(ctx.db, alliance_id).await?;
    let holds = ctx.session.has_corp_role(role) || ctx.session.has_corp_role(CORP_ROLE_DIRECTOR);
    match executor {
        Some(e) if ctx.session.alliance_id == Some(alliance_id) && ctx.session.corp_id == Some(e) && holds => Ok(e),
        _ => Err(access_denied(&format!("only {who} of the executor corporation may do that"))),
    }
}

/// Rows of `select` for one `filter_column` value, as an IndexRowset keyed on
/// the column at `key`.
async fn keyed(ctx: &Ctx<'_>, header: &[&str], select: &str, filter_column: &str, id: i64, key: usize) -> Value {
    let sql = format!(r#"{select} WHERE "{filter_column}" = $1"#);
    keyed_rows(ctx, header, &sql, id, key).await
}

async fn keyed_rows(ctx: &Ctx<'_>, header: &[&str], sql: &str, id: i64, key: usize) -> Value {
    let items = ctx
        .query(sql, &[&(id as i32)])
        .await
        .iter()
        .map(|r| {
            let l = line(r);
            (l[key].clone(), Value::List(l))
        })
        .collect();
    index_rowset(header, header[key], items)
}

fn with(mut corps: Vec<i64>, corp_id: i64) -> Vec<i64> {
    if !corps.contains(&corp_id) {
        corps.push(corp_id);
    }
    corps
}

fn application_event(alliance_id: i64, corp_id: i64, change: Value) -> Value {
    Value::Tuple(vec![Value::Int(alliance_id), Value::Int(corp_id), change])
}

fn member_event(alliance_id: i64, corp_id: i64, change: Value) -> Value {
    Value::Tuple(vec![Value::Int(alliance_id), Value::Int(corp_id), change])
}

/// Queues `event` for every online member of the corporations.
fn tell(world: &World, corps: &[i64], event: &'static str, args: Value) {
    let w = world.lock();
    let clients: Vec<i64> = corps.iter().flat_map(|c| w.corp_members(*c)).collect();
    w.notify(&clients, event, args);
}

fn tell_executor(world: &World, members: &[i64], alliance_id: i64, was: i64, now: i64) {
    let mut c = Vec::new();
    c.push((Value::bytes("executorCorpID"), Value::Tuple(vec![Value::Int(was), Value::Int(now)])));
    tell(world, members, "OnAllianceChanged", Value::Tuple(vec![Value::Int(alliance_id), Value::Dict(c)]));
}

/// A corporation's alliance changed: its cached corporation row, then every
/// member's session.
fn tell_corporation_alliance(world: &World, corp_id: i64, was: Option<i64>, now: Option<i64>) {
    let c = Value::Dict(vec![(Value::bytes("allianceID"), Value::Tuple(vec![Value::int_or_none(was), Value::int_or_none(now)]))]);
    tell(world, &[corp_id], "OnCorporationChanged", Value::Tuple(vec![Value::Int(corp_id), c]));
    world.lock().reaffiliate(&[corp_id], &[]);
}

// ---- relationships, wars and bills ----------------------------------------------------

/// SetRelationship(relationship, toID): NAP, friend, competitor or enemy,
/// toward a corporation or another alliance.
async fn set_relationship(ctx: &Ctx<'_>, alliance_id: i64, relationship: i64, to_id: i64) -> Refused<Value> {
    require_executor_director(ctx, alliance_id).await?;
    if !RELATIONSHIPS.contains(&relationship) {
        return Err(Outcome::custom("That is not a relationship."));
    }
    let known = ctx
        .query_one(
            r#"SELECT 1 WHERE EXISTS (SELECT 1 FROM "corporation" WHERE "corporationID" = $1)
                          OR EXISTS (SELECT 1 FROM alliances WHERE "allianceID" = $1 AND "deleted" = 0)"#,
            &[&(to_id as i32)],
        )
        .await
        .is_some();
    if !known || to_id == alliance_id {
        return Err(Outcome::custom("Relationships are with other corporations and alliances."));
    }
    let old = relationship_line(ctx.db, alliance_id, to_id).await?;
    ctx.db
        .execute(
            r#"INSERT INTO "allianceRelationships" ("fromID", "toID", "relationship") VALUES ($1, $2, $3)
               ON CONFLICT ("fromID", "toID") DO UPDATE SET "relationship" = $3"#,
            &[&(alliance_id as i32), &(to_id as i32), &(relationship as i16)],
        )
        .await
        .map_err(|e| db_failed("setting the relationship", e))?;
    let new = vec![Value::Int(alliance_id), Value::Int(to_id), Value::Int(relationship)];
    tell_relationship(ctx, alliance_id, to_id, change(RELATIONSHIP_HEADER, old.as_deref(), Some(&new))).await
}

async fn delete_relationship(ctx: &Ctx<'_>, alliance_id: i64, to_id: i64) -> Refused<Value> {
    require_executor_director(ctx, alliance_id).await?;
    let Some(old) = relationship_line(ctx.db, alliance_id, to_id).await? else { return Ok(Value::None) };
    ctx.db
        .execute(r#"DELETE FROM "allianceRelationships" WHERE "fromID" = $1 AND "toID" = $2"#, &[&(alliance_id as i32), &(to_id as i32)])
        .await
        .map_err(|e| db_failed("deleting the relationship", e))?;
    tell_relationship(ctx, alliance_id, to_id, change(RELATIONSHIP_HEADER, Some(&old), None)).await
}

async fn relationship_line(db: &impl GenericClient, alliance_id: i64, to_id: i64) -> Refused<Option<Vec<Value>>> {
    let sql = format!(r#"{RELATIONSHIP_SELECT} WHERE "fromID" = $1 AND "toID" = $2"#);
    db.query_opt(sql.as_str(), &[&(alliance_id as i32), &(to_id as i32)])
        .await
        .map(|r| r.as_ref().map(line))
        .map_err(|e| db_failed("reading the relationship", e))
}

/// OnAllianceRelationshipChanged(allianceID, toID, change), to the members.
async fn tell_relationship(ctx: &Ctx<'_>, alliance_id: i64, to_id: i64, change: Value) -> Refused<Value> {
    let members = member_corps(ctx.db, alliance_id).await?;
    let event = Value::Tuple(vec![Value::Int(alliance_id), Value::Int(to_id), change]);
    tell(ctx.world, &members, "OnAllianceRelationshipChanged", event);
    Ok(Value::None)
}

/// DeclareWarAgainst(ownerID): the executor pays for the first week.
async fn declare_war(ctx: &Ctx<'_>, alliance_id: i64, against: i64) -> Refused<Value> {
    let executor = require_executor_director(ctx, alliance_id).await?;
    if member_corps(ctx.db, alliance_id).await?.contains(&against) {
        return Err(Outcome::custom("You cannot declare war on a member of your own alliance."));
    }
    war::declare(ctx, alliance_id, executor, against).await
}

async fn retract_war(ctx: &Ctx<'_>, alliance_id: i64, war_id: i64) -> Refused<Value> {
    require_executor_director(ctx, alliance_id).await?;
    war::retract(ctx, alliance_id, war_id).await
}

async fn mutual_war(ctx: &Ctx<'_>, alliance_id: i64, war_id: i64, mutual: bool) -> Refused<Value> {
    require_executor_director(ctx, alliance_id).await?;
    war::set_mutual(ctx, alliance_id, war_id, mutual).await
}

/// GetBillBalance(billID): what is still owed on one of the alliance's bills.
async fn bill_balance(ctx: &Ctx<'_>, alliance_id: i64, bill_id: i64) -> Refused<Value> {
    let owed = ctx
        .query_one(
            r#"SELECT "amount" FROM "billsReceivable" WHERE "billID" = $1 AND "debtorID" = $2 AND "paid" = 0"#,
            &[&(bill_id as i32), &(alliance_id as i32)],
        )
        .await
        .and_then(|r| crate::svc::rows::float(&r, 0));
    Ok(Value::Double(owed.unwrap_or(0.0)))
}

/// PayBill(billID): the executor's accountants pay the alliance's bill from
/// the executor corporation's wallet.
async fn pay_bill(ctx: &Ctx<'_>, alliance_id: i64, bill_id: i64) -> Refused<Value> {
    let executor = require_executor_role(ctx, alliance_id, CORP_ROLE_ACCOUNTANT, "accountants").await?;
    match wallet::settle_bill(ctx, bill_id, alliance_id, executor).await {
        Outcome::Value(v) => Ok(v),
        refusal => Err(refusal),
    }
}

/// Bills every alliance for its members, once a fortnight from its founding;
/// the founding fee covers the first fortnight. Run by the heartbeat.
pub async fn bill_alliances(db: &Client) {
    let now = crate::bulkdata::filetime_now();
    let sql = r#"SELECT "allianceID", "startDate", "memberCount" FROM alliances WHERE "deleted" = 0"#;
    let rows = match db.query(sql, &[]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("reading the alliances to bill: {}", crate::logging::chain(&e));
            return;
        }
    };
    for r in rows {
        let (alliance_id, founded, members) = (int(&r, 0).unwrap_or(0), int(&r, 1).unwrap_or(now), int(&r, 2).unwrap_or(0));
        let first = founded + MAINTENANCE_PERIOD;
        if let war::Period::Due { until } = war::next_period(db, BILL_TYPE_ALLIANCE_MAINTENANCE, alliance_id, first, now).await {
            let amount = MEMBERSHIP_COST * members.max(1) as f64;
            if war::issue_bill(db, BILL_TYPE_ALLIANCE_MAINTENANCE, alliance_id, amount, until, alliance_id).await.is_some() {
                info!("alliance {alliance_id} billed {amount} ISK for {members} member corporation(s)");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_executor_keeps_the_lead_on_a_tie() {
        assert_eq!(most_backed(&[(1, 1), (2, 1)], 1), Some(1));
        assert_eq!(most_backed(&[(1, 1), (2, 2)], 1), Some(2));
    }

    #[test]
    fn a_departed_executor_hands_over_to_the_most_backed() {
        assert_eq!(most_backed(&[(2, 0), (3, 1)], 1), Some(3));
        assert_eq!(most_backed(&[(2, 0), (3, 0)], 1), Some(2), "nobody backed: the oldest member");
    }
}
