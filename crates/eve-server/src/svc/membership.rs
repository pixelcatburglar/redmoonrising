//! Player corporations: founding one, the application negotiation between a
//! character and a corporation, and leaving one. Whoever changes corporation
//! has their connection told to read its affiliation again
//! (`world::Move::Affiliation`), which sends the session change.

use eve_marshal::Value;
use tokio_postgres::{GenericClient, Transaction};
use tracing::{error, info};

use crate::services::{Call, Ctx, Outcome};
use crate::session::CORP_ROLE_DIRECTOR;
use crate::svc::rows::{change, index_rowset, int, line, lines, rowset, text};
use crate::svc::skills;
use crate::svc::wallet::{self, Entry, Transfer};

/// eveConstants crpApplication*: who last moved an application, and how.
const APPLIED_BY_CHARACTER: i16 = 0;
const RENEGOTIATED_BY_CHARACTER: i16 = 1;
const ACCEPTED_BY_CHARACTER: i16 = 2;
const REJECTED_BY_CHARACTER: i16 = 3;
const REJECTED_BY_CORPORATION: i16 = 4;
const RENEGOTIATED_BY_CORPORATION: i16 = 5;
const ACCEPTED_BY_CORPORATION: i16 = 6;

/// eveConstants corporationStartupCost.
const STARTUP_COST: f64 = 1_599_800.0;
/// market_refTypes: Corporation Registration Fee.
const REF_CORPORATION_REGISTRATION_FEE: i32 = 39;
const CORP_ROLE_PERSONNEL_MANAGER: i64 = 128;
/// Every corpRole* bit eveConstants holds (Director, then bits 7 to 55,
/// Starbase Config among them): what a CEO is given.
pub const ALL_ROLES: i64 = 0x00FF_FFFF_FFFF_FF81;
/// Shares issued to a corporation's founder.
const FOUNDER_SHARES: i32 = 1000;
/// typeCorporationManagement: needed to found a corporation.
const SKILL_CORPORATION_MANAGEMENT: i32 = 3363;
/// The skills that raise a CEO's member limit, and by how much per level
/// (Corporation Management, Megacorp Management, Empire Control, Sovereignty).
const MEMBER_SKILLS: [(i32, i64); 4] = [(3363, 10), (3731, 50), (3732, 200), (12241, 1000)];

const APPLICATION_HEADER: &[&str] =
    &["corporationID", "characterID", "applicationText", "applicationDateTime", "status"];
const APPLICATION_SELECT: &str = r#"
    SELECT "corporationID", "characterID", "applicationText", "applicationDateTime", "status"
      FROM "crpApplications""#;

pub type Refused<T> = std::result::Result<T, Outcome>;

/// corpRegistry calls that change who belongs to which corporation.
pub async fn corp_registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let text_arg = |i: usize| call.arg_text(i).unwrap_or_default();
    let status = |i: usize| call.arg_i64(i).unwrap_or(-1) as i16;
    let result = match call.method.as_str() {
        "GetSuggestedTickerNames" => {
            let tickers = free_tickers(ctx, r#"SELECT "tickerName" FROM "corporation""#, &text_arg(0)).await;
            Ok(rowset(&["tickerName"], tickers.into_iter().map(|t| Value::List(vec![Value::text(t)])).collect()))
        }
        "AddCorporation" => add_corporation(ctx, call).await,
        "GetMyApplications" => Ok(my_applications(ctx).await),
        "GetApplications" => Ok(corp_applications(ctx).await),
        "InsertApplication" => insert_application(ctx, call.arg_id(0), &text_arg(1)).await,
        "UpdateApplication" => answer_offer(ctx, call.arg_id(0), &text_arg(1), status(2)).await,
        "UpdateApplicationOffer" => make_offer(ctx, call.arg_id(0), &text_arg(1), status(2)).await,
        "DeleteApplication" => delete_application(ctx, call.arg_id(0), call.arg_id(1)).await,
        "KickOutMember" => kick_out(ctx, call.arg_id(0)).await,
        "UpdateMember" => {
            let roles: Vec<Option<i64>> = (4..12).map(|i| call.arg_i64(i)).collect();
            update_member(ctx, call.arg_id(0), call.arg_text(1), &roles).await
        }
        "UpdateMembers" => update_members(ctx, call.args.first()).await,
        "CanBeKickedOut" => Ok(Value::Bool(can_be_kicked_out(ctx, call.arg_id(0)).await)),
        _ => return None,
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

// ---- founding --------------------------------------------------------------------------

/// AddCorporation(name, ticker, description, url, taxRate, shape1..3,
/// color1..3, typeface): founded at the station the character is in, which
/// becomes its HQ, with the founder as CEO.
async fn add_corporation(ctx: &Ctx<'_>, call: &Call) -> Refused<Value> {
    let char_id = ctx.session.char_id.ok_or_else(|| Outcome::custom("Select a character first."))?;
    let Some(station_id) = ctx.session.station_id else { return Err(message("CanOnlyCreateCorpInStation")) };
    let name = call.arg_text(0).unwrap_or_default().trim().to_string();
    let ticker = call.arg_text(1).unwrap_or_default().trim().to_string();
    let description = call.arg_text(2).unwrap_or_default();
    let url = call.arg_text(3).unwrap_or_default();
    let tax_rate = call.args.get(4).and_then(|v| v.as_f64()).unwrap_or(0.0).clamp(0.0, 1.0);
    let opt_int = |i: usize| call.arg_i64(i).map(|v| v as i32);
    let typeface = call.arg_text(11);

    if let Some(key) = name_error("Corp", &name) {
        return Err(message(&key));
    }
    if !ticker_ok(&ticker) {
        return Err(message("CorpTickerNameInvalid"));
    }
    if is_ceo(ctx.db, char_id).await? {
        return Err(message("CEOCannotCreateCorporation"));
    }
    let levels = skills::levels(ctx.db, char_id, &MEMBER_SKILLS.map(|(s, _)| s))
        .await
        .map_err(|e| db_failed("reading corporation skills", e))?;
    if levels.get(&SKILL_CORPORATION_MANAGEMENT).copied().unwrap_or(0) < 1 {
        return Err(Outcome::UserError {
            msg: "PlayerCantCreateCorporation".into(),
            args: vec![("cost".into(), Value::Double(STARTUP_COST))],
        });
    }
    let member_limit: i64 = MEMBER_SKILLS.iter().map(|(s, per)| levels.get(s).copied().unwrap_or(0) * per).sum();

    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
    let tx = client.transaction().await.map_err(|e| db_failed("BEGIN", e))?;
    if exists(&tx, r#"SELECT 1 FROM "corporation" WHERE lower("corporationName") = lower($1)"#, &name).await? {
        return Err(message("CorpNameInvalidTaken"));
    }
    if exists(&tx, r#"SELECT 1 FROM "corporation" WHERE lower("tickerName") = lower($1)"#, &ticker).await? {
        return Err(message("CorpTickerNameInvalidTaken"));
    }

    let entry = Entry {
        ref_type: REF_CORPORATION_REGISTRATION_FEE,
        payer: char_id,
        payee: 0,
        arg_id1: "",
        reason: "",
    };
    let balance = wallet::post(&tx, char_id, -STARTUP_COST, &entry).await.map_err(transfer_refused)?;

    let now = crate::bulkdata::filetime_now();
    let corp_id: i32 = tx
        .query_one(
            r#"INSERT INTO "corporation"
                   ("corporationName", "tickerName", "description", "url", "taxRate",
                    "creatorID", "ceoID", "stationID", "raceID", "memberLimit",
                    "shape1", "shape2", "shape3", "color1", "color2", "color3", "typeface")
               SELECT $1, $2, $3, $4, $5, $6, $6, $7, c."raceID", $8, $9, $10, $11, $12, $13, $14, $15
                 FROM "character_" c WHERE c."characterID" = $6
               RETURNING "corporationID""#,
            &[
                &name, &ticker, &description, &url, &tax_rate, &(char_id as i32), &(station_id as i32),
                &(member_limit as i32), &opt_int(5), &opt_int(6), &opt_int(7), &opt_int(8), &opt_int(9),
                &opt_int(10), &typeface,
            ],
        )
        .await
        .map_err(|e| db_failed("inserting the corporation", e))?
        .get(0);
    tx.execute(
        r#"INSERT INTO "crpCharShares" ("characterID", "corporationID", "shares") VALUES ($1, $2, $3)"#,
        &[&(char_id as i32), &corp_id, &FOUNDER_SHARES],
    )
    .await
    .map_err(|e| db_failed("issuing the founder's shares", e))?;
    tx.execute(r#"UPDATE "corporation" SET "shares" = $2 WHERE "corporationID" = $1"#, &[&corp_id, &i64::from(FOUNDER_SHARES)])
        .await
        .map_err(|e| db_failed("recording the shares", e))?;
    let left = change_corporation(&tx, char_id, i64::from(corp_id), ALL_ROLES, now).await?;
    tx.commit().await.map_err(|e| db_failed("COMMIT", e))?;
    member_moved(ctx.world, char_id, left, i64::from(corp_id));

    info!("founded corporation {name:?} [{ticker}] ({corp_id}) at station {station_id}, member limit {member_limit}");
    let world = ctx.world.lock();
    let cash = Value::Tuple(vec![Value::bytes("cash"), Value::Int(char_id), Value::Double(balance)]);
    world.notify(&world.clients_of(char_id), "OnAccountChange", cash);
    world.reaffiliate(&[], &[char_id]);
    Ok(Value::Int(i64::from(corp_id)))
}

// ---- applications ----------------------------------------------------------------------

async fn my_applications(ctx: &Ctx<'_>) -> Value {
    let char_id = ctx.session.char_id.unwrap_or(0) as i32;
    let sql = format!(r#"{APPLICATION_SELECT} WHERE "characterID" = $1 AND "deleted" = 0"#);
    rowset(APPLICATION_HEADER, lines(&ctx.query(&sql, &[&char_id]).await))
}

/// The applications to the caller's corporation, for its personnel managers.
async fn corp_applications(ctx: &Ctx<'_>) -> Value {
    if !may_manage_personnel(ctx) {
        return index_rowset(APPLICATION_HEADER, "characterID", Vec::new());
    }
    let corp = ctx.session.corp_id.unwrap_or(0) as i32;
    let sql = format!(r#"{APPLICATION_SELECT} WHERE "corporationID" = $1 AND "deleted" = 0"#);
    let items = ctx.query(&sql, &[&corp]).await.iter().map(|r| (Value::Int(int(r, 1).unwrap_or(0)), Value::List(line(r)))).collect();
    index_rowset(APPLICATION_HEADER, "characterID", items)
}

/// InsertApplication(corporationID, text): a character asks to join.
async fn insert_application(ctx: &Ctx<'_>, corp_id: i64, note: &str) -> Refused<Value> {
    let char_id = ctx.session.char_id.ok_or_else(|| Outcome::custom("Select a character first."))?;
    if ctx.session.corp_id == Some(corp_id) {
        return Err(Outcome::custom("You are already a member of that corporation."));
    }
    if !is_player_corp(ctx.db, corp_id).await? {
        return Err(Outcome::custom("That corporation does not take applications."));
    }
    if application(ctx.db, corp_id, char_id).await?.is_some() {
        return Err(message("CrpApplicationAlreadyExists"));
    }
    let now = crate::bulkdata::filetime_now();
    ctx.db
        .execute(
            r#"INSERT INTO "crpApplications"
                   ("corporationID", "characterID", "applicationText", "applicationDateTime", "status", "deleted")
               VALUES ($1, $2, $3, $4, $5, 0)
               ON CONFLICT ("corporationID", "characterID") DO UPDATE
                  SET "applicationText" = $3, "applicationDateTime" = $4, "status" = $5,
                      "corporationText" = '', "deleted" = 0"#,
            &[&(corp_id as i32), &(char_id as i32), &note, &now, &APPLIED_BY_CHARACTER],
        )
        .await
        .map_err(|e| db_failed("filing the application", e))?;
    let new = application_line(corp_id, char_id, note, now, APPLIED_BY_CHARACTER);
    info!("{char_id} applied to join corporation {corp_id}");
    tell_application(ctx, corp_id, char_id, change(APPLICATION_HEADER, None, Some(&new)));
    Ok(Value::None)
}

/// UpdateApplicationOffer(characterID, text, status): the corporation answers.
async fn make_offer(ctx: &Ctx<'_>, char_id: i64, note: &str, status: i16) -> Refused<Value> {
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    if !may_manage_personnel(ctx) {
        return Err(access_denied("you do not hold the Personnel Manager role"));
    }
    let Some(old) = application(ctx.db, corp_id, char_id).await? else {
        return Err(Outcome::custom("That application no longer exists."));
    };
    let allowed: &[i16] = match application_status(&old) {
        APPLIED_BY_CHARACTER | RENEGOTIATED_BY_CHARACTER => {
            &[REJECTED_BY_CORPORATION, RENEGOTIATED_BY_CORPORATION, ACCEPTED_BY_CORPORATION]
        }
        REJECTED_BY_CHARACTER => &[RENEGOTIATED_BY_CORPORATION],
        _ => &[],
    };
    if !allowed.contains(&status) {
        return Err(Outcome::custom("The application cannot be moved to that state now."));
    }
    let new = set_application(ctx.db, &old, note, status).await?;
    info!("corporation {corp_id} answered {char_id}'s application with status {status}");
    tell_application(ctx, corp_id, char_id, change(APPLICATION_HEADER, Some(&old), Some(&new)));
    Ok(Value::None)
}

/// UpdateApplication(corporationID, text, status): the character answers the
/// corporation; accepting its offer makes them a member.
async fn answer_offer(ctx: &Ctx<'_>, corp_id: i64, note: &str, status: i16) -> Refused<Value> {
    let char_id = ctx.session.char_id.ok_or_else(|| Outcome::custom("Select a character first."))?;
    let Some(old) = application(ctx.db, corp_id, char_id).await? else {
        return Err(Outcome::custom("That application no longer exists."));
    };
    let allowed: &[i16] = match application_status(&old) {
        REJECTED_BY_CORPORATION => &[RENEGOTIATED_BY_CHARACTER],
        RENEGOTIATED_BY_CORPORATION => &[RENEGOTIATED_BY_CHARACTER, REJECTED_BY_CHARACTER],
        ACCEPTED_BY_CORPORATION => &[ACCEPTED_BY_CHARACTER, REJECTED_BY_CHARACTER, RENEGOTIATED_BY_CHARACTER],
        _ => &[],
    };
    if !allowed.contains(&status) {
        return Err(Outcome::custom("The application cannot be moved to that state now."));
    }
    if status != ACCEPTED_BY_CHARACTER {
        let new = set_application(ctx.db, &old, note, status).await?;
        tell_application(ctx, corp_id, char_id, change(APPLICATION_HEADER, Some(&old), Some(&new)));
        return Ok(Value::None);
    }

    if ctx.session.station_id.is_none() {
        return Err(message("CrpCanNotChangeCorpInSpace"));
    }
    if is_ceo(ctx.db, char_id).await? {
        let names = ctx
            .query_one(
                r#"SELECT o."corporationName", n."corporationName"
                     FROM "character_" c
                     JOIN "corporation" o ON o."corporationID" = c."corporationID"
                     JOIN "corporation" n ON n."corporationID" = $2
                    WHERE c."characterID" = $1"#,
                &[&(char_id as i32), &(corp_id as i32)],
            )
            .await;
        let name = |i: usize| Value::text(names.as_ref().and_then(|r| text(r, i)).unwrap_or_default());
        return Err(Outcome::UserError {
            msg: "CeoCanNotJoinACorp".into(),
            args: vec![("CEOsCorporation".into(), name(0)), ("otherCorporation".into(), name(1))],
        });
    }

    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
    let tx = client.transaction().await.map_err(|e| db_failed("BEGIN", e))?;
    let room = tx
        .query_one(
            r#"SELECT "memberCount", "memberLimit" FROM "corporation" WHERE "corporationID" = $1 FOR UPDATE"#,
            &[&(corp_id as i32)],
        )
        .await
        .map_err(|e| db_failed("reading the member limit", e))?;
    let (count, limit) = (int(&room, 0).unwrap_or(0), int(&room, 1).unwrap_or(0));
    if count >= limit {
        return Err(Outcome::UserError {
            msg: "CorporationMembershipFull".into(),
            args: vec![("limit".into(), Value::Int(limit)), ("count".into(), Value::Int(count))],
        });
    }
    let new = set_application(&tx, &old, note, status).await?;
    let left = change_corporation(&tx, char_id, corp_id, 0, crate::bulkdata::filetime_now()).await?;
    tx.commit().await.map_err(|e| db_failed("COMMIT", e))?;

    info!("{char_id} left corporation {left} and joined {corp_id}");
    member_moved(ctx.world, char_id, left, corp_id);
    tell_application(ctx, corp_id, char_id, change(APPLICATION_HEADER, Some(&old), Some(&new)));
    ctx.world.lock().reaffiliate(&[], &[char_id]);
    Ok(Value::None)
}

/// DeleteApplication(corporationID, characterID): withdrawn by the character,
/// or cleared away by the corporation.
async fn delete_application(ctx: &Ctx<'_>, corp_id: i64, char_id: i64) -> Refused<Value> {
    let own = ctx.session.char_id == Some(char_id);
    let corp_side = ctx.session.corp_id == Some(corp_id) && may_manage_personnel(ctx);
    if !own && !corp_side {
        return Err(access_denied("that application is not yours to delete"));
    }
    let Some(old) = application(ctx.db, corp_id, char_id).await? else { return Ok(Value::None) };
    ctx.db
        .execute(
            r#"DELETE FROM "crpApplications" WHERE "corporationID" = $1 AND "characterID" = $2"#,
            &[&(corp_id as i32), &(char_id as i32)],
        )
        .await
        .map_err(|e| db_failed("deleting the application", e))?;
    tell_application(ctx, corp_id, char_id, change(APPLICATION_HEADER, Some(&old), None));
    Ok(Value::None)
}

/// OnCorporationApplicationChanged(applicantID, corporationID, change), to the
/// applicant and the corporation's members.
fn tell_application(ctx: &Ctx<'_>, corp_id: i64, char_id: i64, change: Value) {
    let world = ctx.world.lock();
    let mut clients = world.clients_of(char_id);
    clients.extend(world.corp_members(corp_id));
    clients.sort_unstable();
    clients.dedup();
    let event = Value::Tuple(vec![Value::Int(char_id), Value::Int(corp_id), change]);
    world.notify(&clients, "OnCorporationApplicationChanged", event);
}

async fn application(db: &impl GenericClient, corp_id: i64, char_id: i64) -> Refused<Option<Vec<Value>>> {
    let sql = format!(r#"{APPLICATION_SELECT} WHERE "corporationID" = $1 AND "characterID" = $2 AND "deleted" = 0"#);
    db.query_opt(sql.as_str(), &[&(corp_id as i32), &(char_id as i32)])
        .await
        .map(|r| r.as_ref().map(line))
        .map_err(|e| db_failed("reading the application", e))
}

fn application_status(line: &[Value]) -> i16 {
    line.get(4).and_then(|v| v.as_i64()).unwrap_or(-1) as i16
}

fn application_line(corp_id: i64, char_id: i64, note: &str, when: i64, status: i16) -> Vec<Value> {
    vec![Value::Int(corp_id), Value::Int(char_id), Value::text(note), Value::Long(when), Value::Int(status.into())]
}

/// Moves an application to `status` with new text; returns its new line.
async fn set_application(db: &impl GenericClient, old: &[Value], note: &str, status: i16) -> Refused<Vec<Value>> {
    let id = |i: usize| old.get(i).and_then(|v| v.as_i64()).unwrap_or(0);
    let (corp_id, char_id) = (id(0), id(1));
    db.execute(
        r#"UPDATE "crpApplications" SET "applicationText" = $3, "status" = $4
            WHERE "corporationID" = $1 AND "characterID" = $2"#,
        &[&(corp_id as i32), &(char_id as i32), &note, &status],
    )
    .await
    .map_err(|e| db_failed("updating the application", e))?;
    let when = old.get(3).and_then(|v| v.as_i64()).unwrap_or(0);
    Ok(application_line(corp_id, char_id, note, when, status))
}

// ---- leaving ---------------------------------------------------------------------------

/// KickOutMember(characterID): a member quits (kicking themselves), or a
/// director puts one out. They go back to the NPC corporation they last
/// belonged to.
pub async fn kick_out(ctx: &Ctx<'_>, char_id: i64) -> Refused<Value> {
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    let own = ctx.session.char_id == Some(char_id);
    if !own && !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err(access_denied("only directors may kick members out"));
    }
    let member = ctx
        .query_one(
            r#"SELECT c."corporationID", co."ceoID" FROM "character_" c
                 JOIN "corporation" co ON co."corporationID" = c."corporationID"
                WHERE c."characterID" = $1"#,
            &[&(char_id as i32)],
        )
        .await;
    let Some(member) = member.filter(|r| int(r, 0) == Some(corp_id)) else { return Err(message("CrpCharNotInThisCorp")) };
    if int(&member, 1) == Some(char_id) {
        return Err(access_denied("the CEO cannot leave the corporation"));
    }
    let in_space = if own { ctx.session.station_id.is_none() } else { ctx.world.lock().is_in_space(char_id) };
    if in_space {
        return Err(message(if own { "CrpCanNotChangeCorpInSpace" } else { "CrpCanNotKickMemberInSpace" }));
    }

    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
    let tx = client.transaction().await.map_err(|e| db_failed("BEGIN", e))?;
    let home = tx
        .query_opt(
            r#"SELECT e."corporationID" FROM "chrEmployment" e
                 JOIN "crpNPCCorporations" n ON n."corporationID" = e."corporationID"
                WHERE e."characterID" = $1
                ORDER BY e."startDate" DESC LIMIT 1"#,
            &[&(char_id as i32)],
        )
        .await
        .map_err(|e| db_failed("finding the NPC corporation to return to", e))?
        .and_then(|r| int(&r, 0))
        .ok_or_else(|| Outcome::custom("There is no NPC corporation for them to go back to."))?;
    change_corporation(&tx, char_id, home, 0, crate::bulkdata::filetime_now()).await?;
    tx.commit().await.map_err(|e| db_failed("COMMIT", e))?;
    member_moved(ctx.world, char_id, corp_id, home);

    if own {
        info!("{char_id} quit corporation {corp_id} for {home}");
    } else {
        info!("{char_id} was kicked out of corporation {corp_id} into {home}");
    }
    ctx.world.lock().reaffiliate(&[], &[char_id]);
    Ok(Value::None)
}

// ---- roles -----------------------------------------------------------------------------

/// The role columns UpdateMember takes, in its argument order after the
/// title, division and squadron (corp_dlg_edit_member.py).
const ROLE_COLUMNS: [&str; 8] = [
    "rolesAtAll", "grantableRoles", "rolesAtHQ", "grantableRolesAtHQ",
    "rolesAtBase", "grantableRolesAtBase", "rolesAtOther", "grantableRolesAtOther",
];

/// UpdateMember(charID, title, divisionID, squadronID, roles, grantableRoles,
/// rolesAtHQ, grantableRolesAtHQ, rolesAtBase, grantableRolesAtBase,
/// rolesAtOther, grantableRolesAtOther, baseID, titleMask); None leaves a
/// value as it is. The CEO and directors manage members; only the CEO makes
/// or manages directors, and nobody changes the CEO.
async fn update_member(ctx: &Ctx<'_>, char_id: i64, title: Option<String>, roles: &[Option<i64>]) -> Refused<Value> {
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    let me = ctx.session.char_id.unwrap_or(0);
    let r = ctx
        .query_one(
            r#"SELECT c."corporationID", co."ceoID", COALESCE(r."rolesAtAll", 0)
                 FROM "character_" c
                 JOIN "corporation" co ON co."corporationID" = c."corporationID"
                 LEFT JOIN "chrCorporationRoles" r ON r."characterID" = c."characterID"
                WHERE c."characterID" = $1"#,
            &[&(char_id as i32)],
        )
        .await
        .filter(|r| int(r, 0) == Some(corp_id))
        .ok_or_else(|| message("CrpCharNotInThisCorp"))?;
    let (ceo, held) = (int(&r, 1).unwrap_or(0), int(&r, 2).unwrap_or(0));
    let caller_is_ceo = me == ceo;
    if !caller_is_ceo && !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err(access_denied("only the CEO and directors may change members"));
    }
    if char_id == ceo {
        return Err(access_denied("the CEO's roles cannot be changed"));
    }
    let director = |mask: i64| mask & CORP_ROLE_DIRECTOR == CORP_ROLE_DIRECTOR;
    if !caller_is_ceo && (director(held) || roles.iter().flatten().any(|m| director(*m))) {
        return Err(access_denied("only the CEO may appoint or manage directors"));
    }

    if let Some(title) = &title {
        ctx.db
            .execute(r#"UPDATE "character_" SET "title" = $2 WHERE "characterID" = $1"#, &[&(char_id as i32), title])
            .await
            .map_err(|e| db_failed("setting the title", e))?;
    }
    if roles.iter().any(Option::is_some) {
        // A member without a roles row starts from none; what is not given stays.
        let id = char_id as i32;
        ctx.db
            .execute(r#"INSERT INTO "chrCorporationRoles" ("characterID") VALUES ($1) ON CONFLICT ("characterID") DO NOTHING"#, &[&id])
            .await
            .map_err(|e| db_failed("adding the roles", e))?;
        let set: Vec<String> =
            ROLE_COLUMNS.iter().enumerate().map(|(i, c)| format!(r#""{c}" = COALESCE(${}, "{c}")"#, i + 2)).collect();
        let sql = format!(r#"UPDATE "chrCorporationRoles" SET {} WHERE "characterID" = $1"#, set.join(", "));
        let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = vec![&id];
        for r in roles.iter().take(ROLE_COLUMNS.len()) {
            params.push(r);
        }
        ctx.db.execute(sql.as_str(), &params).await.map_err(|e| db_failed("setting the roles", e))?;
        if roles.first().copied().flatten().is_some_and(director) {
            info!("{me} made {char_id} a director of {corp_id}");
        }
    }
    info!("{me} updated member {char_id} of corporation {corp_id}");
    ctx.world.lock().reaffiliate(&[], &[char_id]);
    Ok(Value::None)
}

/// UpdateMembers(rows): a util.Rowset of [characterID, title, divisionID,
/// squadronID, roles, grantableRoles, ... titleMask], one UpdateMember each.
async fn update_members(ctx: &Ctx<'_>, rows: Option<&Value>) -> Refused<Value> {
    let lines = match rows {
        Some(Value::Instance { state, .. }) => match state.as_ref() {
            Value::Dict(d) => d.iter().find(|(k, _)| *k == Value::bytes("lines")).and_then(|(_, v)| v.seq()).map(|l| l.to_vec()),
            _ => None,
        },
        _ => None,
    };
    for l in lines.unwrap_or_default() {
        let Some(l) = l.seq() else { continue };
        let char_id = l.first().and_then(|v| v.as_i64()).unwrap_or(0);
        let title = l.get(1).and_then(|v| v.as_string());
        let roles: Vec<Option<i64>> = (4..12).map(|i| l.get(i).and_then(|v| v.as_i64())).collect();
        update_member(ctx, char_id, title, &roles).await?;
    }
    Ok(Value::None)
}

async fn can_be_kicked_out(ctx: &Ctx<'_>, char_id: i64) -> bool {
    let corp_id = ctx.session.corp_id.unwrap_or(0) as i32;
    ctx.query_one(
        r#"SELECT 1 FROM "character_" c JOIN "corporation" co ON co."corporationID" = c."corporationID"
            WHERE c."characterID" = $1 AND c."corporationID" = $2 AND co."ceoID" <> $1"#,
        &[&(char_id as i32), &corp_id],
    )
    .await
    .is_some()
}

// ---- shared ----------------------------------------------------------------------------

/// Moves a character into `corp_id` with `roles`, inside the caller's
/// transaction: membership counts, the employment record and the roles.
/// Returns the corporation they left.
pub async fn change_corporation(tx: &Transaction<'_>, char_id: i64, corp_id: i64, roles: i64, now: i64) -> Refused<i64> {
    let char32 = char_id as i32;
    let old = tx
        .query_one(
            r#"UPDATE "character_" n SET "corporationID" = $2, "corporationDateTime" = $3
                 FROM "character_" o
                WHERE n."characterID" = $1 AND o."characterID" = $1
               RETURNING o."corporationID""#,
            &[&char32, &(corp_id as i32), &now],
        )
        .await
        .map_err(|e| db_failed("moving the character", e))?;
    let old_corp = int(&old, 0).unwrap_or(0);
    let counts = [(old_corp, -1i32), (corp_id, 1)];
    for (corp, delta) in counts {
        tx.execute(
            r#"UPDATE "corporation" SET "memberCount" = GREATEST(0, "memberCount" + $2) WHERE "corporationID" = $1"#,
            &[&(corp as i32), &delta],
        )
        .await
        .map_err(|e| db_failed("counting members", e))?;
    }
    tx.execute(
        r#"INSERT INTO "chrEmployment" ("characterID", "corporationID", "startDate", "deleted") VALUES ($1, $2, $3, 0)"#,
        &[&char32, &(corp_id as i32), &now],
    )
    .await
    .map_err(|e| db_failed("recording the employment", e))?;
    tx.execute(
        r#"INSERT INTO "chrCorporationRoles" ("characterID", "rolesAtAll", "grantableRoles")
           VALUES ($1, $2, $2)
           ON CONFLICT ("characterID") DO UPDATE
              SET "corprole" = 0, "rolesAtAll" = $2, "rolesAtBase" = 0, "rolesAtHQ" = 0,
                  "rolesAtOther" = 0, "grantableRoles" = $2, "grantableRolesAtHQ" = 0,
                  "grantableRolesAtBase" = 0, "grantableRolesAtOther" = 0"#,
        &[&char32, &roles],
    )
    .await
    .map_err(|e| db_failed("setting the roles", e))?;
    Ok(old_corp)
}

/// Tells both corporations' members, and the character, that they moved, so
/// open member lists follow (bco_members.py:170). Called after the commit.
pub fn member_moved(world: &crate::world::World, char_id: i64, old_corp: i64, new_corp: i64) {
    let w = world.lock();
    let mut clients: Vec<i64> = w.corp_members(old_corp);
    clients.extend(w.corp_members(new_corp));
    clients.extend(w.clients_of(char_id));
    clients.sort_unstable();
    clients.dedup();
    let change = Value::Dict(vec![(Value::bytes("corporationID"), Value::Tuple(vec![Value::Int(old_corp), Value::Int(new_corp)]))]);
    w.notify(&clients, "OnCorporationMemberChanged", Value::Tuple(vec![Value::Int(char_id), change]));
}

/// Whether the character is the CEO of a player corporation.
pub async fn is_ceo(db: &impl GenericClient, char_id: i64) -> Refused<bool> {
    db.query_opt(
        r#"SELECT 1 FROM "corporation" co
            WHERE co."ceoID" = $1
              AND NOT EXISTS (SELECT 1 FROM "crpNPCCorporations" n WHERE n."corporationID" = co."corporationID")"#,
        &[&(char_id as i32)],
    )
    .await
    .map(|r| r.is_some())
    .map_err(|e| db_failed("checking for a CEO", e))
}

pub async fn is_player_corp(db: &impl GenericClient, corp_id: i64) -> Refused<bool> {
    db.query_opt(
        r#"SELECT 1 FROM "corporation" co
            WHERE co."corporationID" = $1 AND co."deleted" = 0
              AND NOT EXISTS (SELECT 1 FROM "crpNPCCorporations" n WHERE n."corporationID" = co."corporationID")"#,
        &[&(corp_id as i32)],
    )
    .await
    .map(|r| r.is_some())
    .map_err(|e| db_failed("reading the corporation", e))
}

/// Whether `sql` (taking one text parameter) finds a row: a name in use.
pub async fn exists(db: &impl GenericClient, sql: &str, value: &str) -> Refused<bool> {
    db.query_opt(sql, &[&value]).await.map(|r| r.is_some()).map_err(|e| db_failed("checking a name", e))
}

fn may_manage_personnel(ctx: &Ctx<'_>) -> bool {
    ctx.session.has_corp_role(CORP_ROLE_PERSONNEL_MANAGER) || ctx.session.has_corp_role(CORP_ROLE_DIRECTOR)
}

/// The message key for what is wrong with a corporation or alliance name
/// (`prefix` "Corp" or "Alliance": CorpNameInvalidMinLength and so on).
pub fn name_error(prefix: &str, name: &str) -> Option<String> {
    let chars: Vec<char> = name.chars().collect();
    let problem = if chars.len() < 4 {
        "MinLength"
    } else if chars.len() > 24 {
        "MaxLength"
    } else if !chars.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '.' | '\'')) {
        "SomeChar"
    } else if !chars[0].is_ascii_alphanumeric() {
        "FirstChar"
    } else if !chars[chars.len() - 1].is_ascii_alphanumeric() && chars[chars.len() - 1] != '.' {
        "LastChar"
    } else {
        return None;
    };
    Some(format!("{prefix}NameInvalid{problem}"))
}

/// A corporation ticker or alliance short name: one to five letters, digits,
/// dots, dashes or underscores.
pub fn ticker_ok(ticker: &str) -> bool {
    (1..=5).contains(&ticker.chars().count())
        && ticker.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Tickers made from a name: its initials, its first letters, then those
/// with a digit; the ones `taken_sql` (a query of the names in use) does
/// not list.
pub async fn free_tickers(ctx: &Ctx<'_>, taken_sql: &str, name: &str) -> Vec<String> {
    let candidates = ticker_candidates(name);
    let sql = format!(r#"SELECT upper(t) FROM ({taken_sql}) AS used(t) WHERE upper(t) = ANY($1)"#);
    let taken: Vec<String> = ctx.query(&sql, &[&candidates]).await.iter().filter_map(|r| text(r, 0)).collect();
    candidates.into_iter().filter(|c| !taken.contains(c)).take(8).collect()
}

fn ticker_candidates(name: &str) -> Vec<String> {
    let words: Vec<String> = name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_ascii_uppercase())
        .collect();
    let letters: String = words.concat();
    let initials: String = words.iter().filter_map(|w| w.chars().next()).take(5).collect();
    let mut out: Vec<String> = Vec::new();
    let mut add = |t: String| {
        if ticker_ok(&t) && !out.contains(&t) {
            out.push(t);
        }
    };
    if !initials.is_empty() {
        add(initials.clone());
    }
    for n in (2..=5).rev() {
        add(letters.chars().take(n).collect());
    }
    let stem: String = initials.chars().take(4).collect();
    if !stem.is_empty() {
        for d in 1..=9 {
            add(format!("{stem}{d}"));
        }
    }
    out
}

pub fn message(key: &str) -> Outcome {
    Outcome::UserError { msg: key.into(), args: Vec::new() }
}

pub fn access_denied(reason: &str) -> Outcome {
    Outcome::UserError { msg: "CrpAccessDenied".into(), args: vec![("reason".into(), Value::text(reason))] }
}

pub fn db_failed(what: &str, e: tokio_postgres::Error) -> Outcome {
    error!("{what}: {}", crate::logging::chain(&e));
    Outcome::custom("The corporation registry could not do that just now.")
}

pub fn transfer_refused(t: Transfer) -> Outcome {
    match t {
        Transfer::NotEnough { need, have } => Outcome::not_enough_money(need, have),
        Transfer::NoSuchOwner => Outcome::custom("That wallet does not exist."),
        Transfer::Db(e) => db_failed("paying", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked_as_the_client_messages_describe() {
        assert_eq!(name_error("Corp", "Abc").as_deref(), Some("CorpNameInvalidMinLength"));
        assert_eq!(name_error("Corp", &"x".repeat(25)).as_deref(), Some("CorpNameInvalidMaxLength"));
        assert_eq!(name_error("Alliance", " Abcd").as_deref(), Some("AllianceNameInvalidFirstChar"));
        assert_eq!(name_error("Corp", "Abcd!").as_deref(), Some("CorpNameInvalidSomeChar"));
        assert_eq!(name_error("Corp", "Mining Co."), None);
    }

    #[test]
    fn tickers_start_with_the_initials() {
        let t = ticker_candidates("Red Moon Rising");
        assert_eq!(t[0], "RMR");
        assert!(t.contains(&"REDMO".to_string()));
        assert!(t.iter().all(|t| ticker_ok(t)));
        assert!(t.contains(&"RMR1".to_string()));
    }

    #[test]
    fn a_ceo_holds_every_role_the_constants_define() {
        for role in [1, 128, 256, 512, 1024, 2048, 4096, 8192, 1 << 20, 1 << 27, 1 << 31, 1 << 41, 1 << 53, 1 << 55] {
            assert_eq!(ALL_ROLES & role, role, "role {role}");
        }
    }
}
