//! Corporation votes (corp_ui_votes.py) and the actions they sanction
//! (corp_ui_sanctionableactions.py).
//!
//! A director or the CEO puts a question to the shareholders, with options;
//! each shareholder votes once, weighted by the shares they hold. When the
//! vote closes ([`close_votes`], from the heartbeat) the option with the most
//! shares behind it wins (the first on a tie) and is recorded as a sanctioned
//! action. For a war, an expulsion, new shares or a blueprint lockdown the
//! CEO then has a week to put it into effect (UpdateSanctionedAction); a
//! general vote's decision is simply recorded.
//!
//! CEO elections are votes too: a member with Corporation Management runs
//! (ProposeCEOVote), the shareholders choose between the candidate and the
//! CEO there is, and a winning candidate takes over as the vote closes.
//! While one is open no other vote may be put. A CEO may also resign
//! (ResignFromCEO), handing over to a member of their choice, or closing
//! the corporation if nobody else is left.

use eve_marshal::Value;
use tokio_postgres::{Client, GenericClient};
use tracing::{error, info};

use crate::services::{Call, Ctx, Outcome};
use crate::session::CORP_ROLE_DIRECTOR;
use crate::svc::membership::{access_denied, db_failed, message, Refused};
use crate::svc::rows::{change, index_rowset, int, line, text};
use crate::world::World;

/// eveConstants vote*.
const VOTE_CEO: i64 = 0;
const VOTE_WAR: i64 = 1;
const VOTE_SHARES: i64 = 2;
const VOTE_KICK_MEMBER: i64 = 3;
const VOTE_ITEM_LOCKDOWN: i64 = 5;
const VOTE_ITEM_UNLOCK: i64 = 6;
/// How long the CEO has to act on a sanctioned action.
const ACT_WITHIN: i64 = 7 * 24 * 3_600 * 10_000_000;

const CASE_HEADER: &[&str] =
    &["voteCaseID", "corporationID", "voteType", "voteCaseText", "description", "startDateTime", "endDateTime"];
const CASE_SELECT: &str = r#"SELECT "voteCaseID", "corporationID", "voteType", "voteCaseText", "description",
                                    "startDateTime", "endDateTime" FROM "crpVoteCases""#;
const OPTION_HEADER: &[&str] = &["voteCaseID", "optionID", "optionText", "parameter", "parameter1", "parameter2", "votesFor"];
const VOTE_HEADER: &[&str] = &["voteCaseID", "characterID", "optionID"];
const SANCTION_HEADER: &[&str] = &[
    "voteCaseID", "corporationID", "voteType", "optionID", "parameter", "parameter1", "parameter2",
    "expires", "actedUpon", "inEffect", "timeActedUpon", "timeRescended", "reason",
];
const SANCTION_SELECT: &str = r#"
    SELECT s."voteCaseID", s."corporationID", c."voteType", s."optionID", o."parameter", o."parameter1", o."parameter2",
           s."expires", s."actedUpon", s."inEffect", s."timeActedUpon", s."timeRescended", s."reason"
      FROM "crpSanctionedActions" s
      JOIN "crpVoteCases" c ON c."voteCaseID" = s."voteCaseID"
      LEFT JOIN "crpVoteOptions" o ON o."voteCaseID" = s."voteCaseID" AND o."optionID" = s."optionID""#;

pub async fn corp_registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let corp = call.arg_id(0);
    let reads = matches!(
        call.method.as_str(),
        "GetVoteCasesByCorporation" | "GetVoteCaseOptions" | "GetVotes" | "GetSanctionedActionsByCorporation"
    );
    if reads {
        if let Err(refusal) = viewer(ctx, corp).await {
            return Some(refusal);
        }
    }
    let result = match call.method.as_str() {
        "GetVoteCasesByCorporation" => Ok(cases(ctx, corp).await),
        "GetVoteCaseOptions" => Ok(options(ctx, call.arg_id(1)).await),
        "GetVotes" => Ok(votes(ctx, call.arg_id(1)).await),
        "GetSanctionedActionsByCorporation" => Ok(sanctions(ctx, corp).await),
        "CanViewVotes" => Ok(Value::Bool(viewer(ctx, corp).await.is_ok())),
        "InsertVoteCase" => propose(ctx, call).await,
        "InsertVote" => vote(ctx, corp, call.arg_id(1), call.arg_id(2)).await,
        "UpdateSanctionedAction" => {
            let in_effect = call.arg_i64(1).unwrap_or(0) != 0;
            act(ctx, call.arg_id(0), in_effect, &call.arg_text(2).unwrap_or_default()).await
        }
        "MemberCanRunForCEO" => Ok(Value::Int(can_run(ctx).await.is_ok().into())),
        "ResignFromCEO" => resign(ctx, call.arg_i64(0)).await,
        _ => return None,
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

/// Who may see a corporation's votes: its directors, and its shareholders.
async fn viewer(ctx: &Ctx<'_>, corp: i64) -> Refused<()> {
    if ctx.session.corp_id == Some(corp) && ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Ok(());
    }
    if shares_of(ctx.db, corp, ctx.session.char_id.unwrap_or(0)).await > 0 {
        return Ok(());
    }
    Err(access_denied("you must be the CEO, a director or a shareholder"))
}

async fn shares_of(db: &impl GenericClient, corp: i64, char_id: i64) -> i64 {
    let sql = r#"SELECT COALESCE(sum("shares"), 0)::bigint FROM "crpCharShares" WHERE "corporationID" = $1 AND "characterID" = $2"#;
    db.query_opt(sql, &[&(corp as i32), &(char_id as i32)]).await.ok().flatten().map_or(0, |r| r.get(0))
}

async fn cases(ctx: &Ctx<'_>, corp: i64) -> Value {
    let sql = format!(r#"{CASE_SELECT} WHERE "corporationID" = $1 ORDER BY "voteCaseID""#);
    keyed(ctx, CASE_HEADER, &sql, corp, 0).await
}

/// The options, with the shares behind each so far.
async fn options(ctx: &Ctx<'_>, case: i64) -> Value {
    let sql = r#"SELECT o."voteCaseID", o."optionID", o."optionText", o."parameter", o."parameter1", o."parameter2",
                        COALESCE((SELECT sum(sh."shares") FROM "crpVotes" v
                                    JOIN "crpVoteCases" c ON c."voteCaseID" = v."voteCaseID"
                                    JOIN "crpCharShares" sh ON sh."characterID" = v."characterID" AND sh."corporationID" = c."corporationID"
                                   WHERE v."voteCaseID" = o."voteCaseID" AND v."optionID" = o."optionID"), 0)::integer
                   FROM "crpVoteOptions" o WHERE o."voteCaseID" = $1 ORDER BY o."optionID""#;
    keyed(ctx, OPTION_HEADER, sql, case, 1).await
}

async fn votes(ctx: &Ctx<'_>, case: i64) -> Value {
    let sql = r#"SELECT "voteCaseID", "characterID", "optionID" FROM "crpVotes" WHERE "voteCaseID" = $1 ORDER BY "characterID""#;
    keyed(ctx, VOTE_HEADER, sql, case, 1).await
}

async fn sanctions(ctx: &Ctx<'_>, corp: i64) -> Value {
    let sql = format!(r#"{SANCTION_SELECT} WHERE s."corporationID" = $1 ORDER BY s."voteCaseID""#);
    keyed(ctx, SANCTION_HEADER, &sql, corp, 0).await
}

async fn keyed(ctx: &Ctx<'_>, header: &[&str], sql: &str, id: i64, key: usize) -> Value {
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

/// InsertVoteCase(text, description, corporationID, voteType, options,
/// startDateTime, endDateTime); options a util.Rowset of [optionText,
/// parameter, parameter1, parameter2].
async fn propose(ctx: &Ctx<'_>, call: &Call) -> Refused<Value> {
    let corp = call.arg_id(2);
    let (text, description) = (call.arg_text(0).unwrap_or_default(), call.arg_text(1).unwrap_or_default());
    let vote_type = call.arg_i64(3).unwrap_or(-1);
    if vote_type == VOTE_CEO {
        can_run(ctx).await?;
    } else {
        if ctx.session.corp_id != Some(corp) || !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
            return Err(message("CrpOnlyDirectorsCanProposeVotes"));
        }
        if ceo_vote_open(ctx.db, corp).await {
            return Err(message("CEOCantAddVoteInCEOVotePeriod"));
        }
    }
    let now = crate::bulkdata::filetime_now();
    let start = call.arg_i64(5).unwrap_or(now).max(now);
    let end = call.arg_i64(6).unwrap_or(start).max(start);
    let lines = rowset_lines(call.args.get(4));
    if lines.len() < 2 {
        return Err(Outcome::custom("A vote needs at least two options."));
    }
    // A candidate runs for themselves only.
    let me = ctx.session.char_id.unwrap_or(0);
    if vote_type == VOTE_CEO && !lines.iter().all(|l| matches!(l.get(1).and_then(|v| v.as_i64()), Some(0) | None) || l.get(1).and_then(|v| v.as_i64()) == Some(me)) {
        return Err(message("CantRunForCEOAtTheMoment"));
    }
    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
    let tx = client.transaction().await.map_err(|e| db_failed("BEGIN", e))?;
    let case: i32 = tx
        .query_one(
            r#"INSERT INTO "crpVoteCases" ("voteCaseID", "corporationID", "voteType", "voteCaseText", "description", "startDateTime", "endDateTime")
               VALUES (nextval('dyn_id_seq'), $1, $2, $3, $4, $5, $6) RETURNING "voteCaseID""#,
            &[&(corp as i32), &(vote_type as i16), &text, &description, &start, &end],
        )
        .await
        .map_err(|e| db_failed("recording the vote", e))?
        .get(0);
    for (i, l) in lines.iter().enumerate() {
        let param = |k: usize| l.get(k).and_then(|v| v.as_i64()).map(|v| v as i32);
        tx.execute(
            r#"INSERT INTO "crpVoteOptions" ("voteCaseID", "optionID", "optionText", "parameter", "parameter1", "parameter2")
               VALUES ($1, $2, $3, $4, $5, $6)"#,
            &[&case, &(i as i32 + 1), &l.first().and_then(|v| v.as_string()).unwrap_or_default(), &param(1), &param(2), &param(3)],
        )
        .await
        .map_err(|e| db_failed("recording an option", e))?;
    }
    tx.commit().await.map_err(|e| db_failed("COMMIT", e))?;
    let new = vec![
        Value::Int(case.into()), Value::Int(corp), Value::Int(vote_type), Value::text(&text), Value::text(&description),
        Value::Long(start), Value::Long(end),
    ];
    info!("corporation {corp} put vote {case} ({text:?}) to its shareholders");
    tell(ctx.world, corp, "OnCorporationVoteCaseChanged", vec![Value::Int(corp), Value::Int(case.into()), change(CASE_HEADER, None, Some(&new))]);
    Ok(Value::None)
}

/// The lines of a util.Rowset argument.
fn rowset_lines(v: Option<&Value>) -> Vec<Vec<Value>> {
    let Some(Value::Instance { state, .. }) = v else { return Vec::new() };
    let Value::Dict(d) = state.as_ref() else { return Vec::new() };
    d.iter()
        .find(|(k, _)| *k == Value::bytes("lines"))
        .and_then(|(_, v)| v.seq())
        .map(|lines| lines.iter().filter_map(|l| l.seq().map(|l| l.to_vec())).collect())
        .unwrap_or_default()
}

/// InsertVote(corporationID, voteCaseID, optionID): once per shareholder,
/// while the vote is open.
async fn vote(ctx: &Ctx<'_>, corp: i64, case: i64, option: i64) -> Refused<Value> {
    let char_id = ctx.session.char_id.unwrap_or(0);
    if shares_of(ctx.db, corp, char_id).await <= 0 {
        return Err(access_denied("only shareholders may vote"));
    }
    let now = crate::bulkdata::filetime_now();
    let open = ctx
        .query_one(
            r#"SELECT 1 FROM "crpVoteCases" c JOIN "crpVoteOptions" o ON o."voteCaseID" = c."voteCaseID"
                WHERE c."voteCaseID" = $1 AND c."corporationID" = $2 AND o."optionID" = $3
                  AND c."startDateTime" <= $4 AND c."endDateTime" > $4"#,
            &[&(case as i32), &(corp as i32), &(option as i32), &now],
        )
        .await
        .is_some();
    if !open {
        return Err(Outcome::custom("That vote is not open."));
    }
    let added = ctx
        .db
        .execute(
            r#"INSERT INTO "crpVotes" ("voteCaseID", "characterID", "optionID") VALUES ($1, $2, $3) ON CONFLICT DO NOTHING"#,
            &[&(case as i32), &(char_id as i32), &(option as i32)],
        )
        .await
        .map_err(|e| db_failed("recording the vote", e))?;
    if added == 0 {
        return Err(message("CrpAlreadyVoted"));
    }
    let new = vec![Value::Int(case), Value::Int(char_id), Value::Int(option)];
    tell(ctx.world, corp, "OnCorporationVoteChanged", vec![Value::Int(corp), Value::Int(case), Value::Int(char_id), change(VOTE_HEADER, None, Some(&new))]);
    Ok(Value::None)
}

/// UpdateSanctionedAction(voteCaseID, inEffect, reason): the CEO puts a
/// sanctioned action into effect, or rescinds it.
async fn act(ctx: &Ctx<'_>, case: i64, in_effect: bool, reason: &str) -> Refused<Value> {
    let corp = ctx.session.corp_id.unwrap_or(0);
    let ceo = ctx
        .query_one(r#"SELECT "ceoID" FROM "corporation" WHERE "corporationID" = $1"#, &[&(corp as i32)])
        .await
        .and_then(|r| int(&r, 0));
    if ceo != ctx.session.char_id {
        return Err(access_denied("only the CEO may act on what the shareholders sanctioned"));
    }
    let sql = format!(r#"{SANCTION_SELECT} WHERE s."voteCaseID" = $1 AND s."corporationID" = $2"#);
    let old = ctx
        .query_one(&sql, &[&(case as i32), &(corp as i32)])
        .await
        .map(|r| line(&r))
        .ok_or_else(|| Outcome::custom("There is no such sanctioned action."))?;
    let at = |i: usize| old[i].as_i64();
    let now = crate::bulkdata::filetime_now();
    if in_effect {
        if at(8) == Some(1) {
            return Ok(Value::None);
        }
        if at(7).is_some_and(|expires| expires < now) {
            return Err(Outcome::custom("That sanctioned action has expired."));
        }
        let (vote_type, parameter) = (at(2).unwrap_or(-1), at(4).unwrap_or(0));
        if parameter == 0 {
            return Err(Outcome::custom("The shareholders decided against it."));
        }
        carry_out(ctx, corp, vote_type, parameter, at(5), at(6)).await?;
    }
    let saved = ctx
        .db
        .execute(
            r#"UPDATE "crpSanctionedActions"
                  SET "inEffect" = $2, "actedUpon" = 1, "reason" = $3,
                      "timeActedUpon" = CASE WHEN $2 = 1 THEN $4 ELSE "timeActedUpon" END,
                      "timeRescended" = CASE WHEN $2 = 0 THEN $4 ELSE "timeRescended" END
                WHERE "voteCaseID" = $1"#,
            &[&(case as i32), &i16::from(in_effect), &reason, &now],
        )
        .await;
    saved.map_err(|e| db_failed("recording the action", e))?;
    let mut new = old.clone();
    new[8] = Value::Int(1);
    new[9] = Value::Int(in_effect.into());
    if in_effect { new[10] = Value::Long(now) } else { new[11] = Value::Long(now) }
    new[12] = Value::text(reason);
    info!("CEO of {corp} {} sanctioned action {case}", if in_effect { "carried out" } else { "rescinded" });
    tell(ctx.world, corp, "OnSanctionedActionChanged", vec![Value::Int(corp), Value::Int(case), change(SANCTION_HEADER, Some(&old), Some(&new))]);
    Ok(Value::None)
}

/// What a sanctioned action does.
async fn carry_out(ctx: &Ctx<'_>, corp: i64, vote_type: i64, parameter: i64, type_id: Option<i64>, location: Option<i64>) -> Refused<()> {
    match vote_type {
        VOTE_WAR => {
            if ctx.session.alliance_id.is_some() {
                return Err(Outcome::custom("Your alliance declares war for you."));
            }
            crate::svc::war::declare(ctx, corp, corp, parameter).await.map(|_| ())
        }
        VOTE_KICK_MEMBER => crate::svc::membership::kick_out(ctx, parameter).await.map(|_| ()),
        VOTE_SHARES => ctx
            .db
            .execute(r#"UPDATE "corporation" SET "shares" = "shares" + $2 WHERE "corporationID" = $1"#, &[&(corp as i32), &parameter])
            .await
            .map(|_| ())
            .map_err(|e| db_failed("creating shares", e)),
        VOTE_ITEM_LOCKDOWN => ctx
            .db
            .execute(
                r#"INSERT INTO "crpItemLocks" ("corporationID", "itemID", "locationID", "typeID", "lockedAt") VALUES ($1, $2, $3, $4, $5)"#,
                &[&(corp as i32), &(parameter as i32), &location.map(|l| l as i32), &type_id.map(|t| t as i32), &crate::bulkdata::filetime_now()],
            )
            .await
            .map(|_| ())
            .map_err(|e| db_failed("locking the item", e)),
        VOTE_ITEM_UNLOCK => ctx
            .db
            .execute(r#"DELETE FROM "crpItemLocks" WHERE "corporationID" = $1 AND "itemID" = $2"#, &[&(corp as i32), &(parameter as i32)])
            .await
            .map(|_| ())
            .map_err(|e| db_failed("unlocking the item", e)),
        _ => Ok(()),
    }
}

// ---- CEO --------------------------------------------------------------------------------

/// Whether the caller may run for CEO: a member of a player corporation who
/// is not its CEO, has Corporation Management, and no election is open.
async fn can_run(ctx: &Ctx<'_>) -> Refused<()> {
    let (corp, me) = (ctx.session.corp_id.unwrap_or(0), ctx.session.char_id.unwrap_or(0));
    let ceo = ctx
        .query_one(r#"SELECT "ceoID" FROM "corporation" WHERE "corporationID" = $1"#, &[&(corp as i32)])
        .await
        .and_then(|r| int(&r, 0));
    let player = crate::svc::membership::is_player_corp(ctx.db, corp).await.unwrap_or(false);
    let skilled = crate::svc::skills::levels(ctx.db, me, &[SKILL_CORPORATION_MANAGEMENT])
        .await
        .map(|l| l.get(&SKILL_CORPORATION_MANAGEMENT).copied().unwrap_or(0) > 0)
        .unwrap_or(false);
    if !player || ceo == Some(me) || !skilled || ceo_vote_open(ctx.db, corp).await {
        return Err(message("CantRunForCEOAtTheMoment"));
    }
    Ok(())
}

/// typeCorporationManagement.
const SKILL_CORPORATION_MANAGEMENT: i32 = 3363;

async fn ceo_vote_open(db: &impl GenericClient, corp: i64) -> bool {
    let sql = r#"SELECT 1 FROM "crpVoteCases" WHERE "corporationID" = $1 AND "voteType" = $2 AND "endDateTime" > $3"#;
    db.query_opt(sql, &[&(corp as i32), &(VOTE_CEO as i16), &crate::bulkdata::filetime_now()]).await.ok().flatten().is_some()
}

/// ResignFromCEO(newCeoID=None): without a successor, the members to choose
/// from (or, with nobody else left, the corporation closes); with one, they
/// take over.
async fn resign(ctx: &Ctx<'_>, successor: Option<i64>) -> Refused<Value> {
    let (corp, me) = (ctx.session.corp_id.unwrap_or(0), ctx.session.char_id.unwrap_or(0));
    let ceo = ctx
        .query_one(r#"SELECT "ceoID" FROM "corporation" WHERE "corporationID" = $1"#, &[&(corp as i32)])
        .await
        .and_then(|r| int(&r, 0));
    if ceo != Some(me) {
        return Err(message("OnlyCEOCanResign"));
    }
    if ctx.session.station_id.is_none() {
        return Err(message("CrpCanNotChangeCorpInSpace"));
    }
    let others: Vec<i64> = ctx
        .query(r#"SELECT "characterID" FROM "character_" WHERE "corporationID" = $1 AND "characterID" <> $2"#, &[&(corp as i32), &(me as i32)])
        .await
        .iter()
        .filter_map(|r| int(r, 0))
        .collect();
    match successor {
        None if others.is_empty() => close(ctx, corp, me).await.map(|_| Value::None),
        None => Ok(Value::List(others.into_iter().map(Value::Int).collect())),
        Some(next) if others.contains(&next) => {
            hand_over(ctx.db, ctx.world, corp, me, next).await?;
            announce_resignation(ctx, corp, me, next).await;
            Ok(Value::None)
        }
        Some(_) => Err(message("CrpCharNotInThisCorp")),
    }
}

/// A new CEO takes over: they get every role, and both sessions are told.
pub async fn hand_over(db: &impl GenericClient, world: &World, corp: i64, old: i64, new: i64) -> Refused<()> {
    db.execute(r#"UPDATE "corporation" SET "ceoID" = $2 WHERE "corporationID" = $1"#, &[&(corp as i32), &(new as i32)])
        .await
        .map_err(|e| db_failed("changing the CEO", e))?;
    let all = crate::svc::membership::ALL_ROLES;
    db.execute(
        r#"INSERT INTO "chrCorporationRoles" ("characterID", "rolesAtAll", "grantableRoles") VALUES ($1, $2, $2)
           ON CONFLICT ("characterID") DO UPDATE SET "rolesAtAll" = $2, "grantableRoles" = $2"#,
        &[&(new as i32), &all],
    )
    .await
    .map_err(|e| db_failed("giving the CEO their roles", e))?;
    info!("{new} is the new CEO of corporation {corp}, after {old}");
    let c = Value::Dict(vec![(Value::bytes("ceoID"), Value::Tuple(vec![Value::Int(old), Value::Int(new)]))]);
    tell(world, corp, "OnCorporationChanged", vec![Value::Int(corp), c]);
    world.lock().reaffiliate(&[], &[old, new]);
    Ok(())
}

/// A mail to the corporation's mailbox, from the outgoing CEO, naming who
/// takes over.
async fn announce_resignation(ctx: &Ctx<'_>, corp: i64, old: i64, new: i64) {
    let sql = r#"SELECT (SELECT "characterName" FROM "character_" WHERE "characterID" = $1),
                        (SELECT "characterName" FROM "character_" WHERE "characterID" = $2),
                        (SELECT "corporationName" FROM "corporation" WHERE "corporationID" = $3)"#;
    let Some(r) = ctx.query_one(sql, &[&(old as i32), &(new as i32), &(corp as i32)]).await else { return };
    let name = |i| text(&r, i).unwrap_or_default();
    let subject = format!("{} has resigned as CEO", name(0));
    let body = format!("{} has resigned as CEO of {}.<br>{} is the new CEO.", name(0), name(2), name(1));
    if let Err(e) = crate::svc::chat::deliver(ctx.db, ctx.world, old, &[corp], &subject, &body).await {
        error!("mailing corporation {corp} about its new CEO: {}", crate::logging::chain(&e));
    }
}

/// The last member resigns: the corporation closes and they go back to
/// their NPC corporation.
async fn close(ctx: &Ctx<'_>, corp: i64, me: i64) -> Refused<()> {
    if ctx.session.alliance_id.is_some() {
        return Err(message("CanNotDestroyCorpInAlliance"));
    }
    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
    let tx = client.transaction().await.map_err(|e| db_failed("BEGIN", e))?;
    let home = tx
        .query_opt(
            r#"SELECT e."corporationID" FROM "chrEmployment" e
                 JOIN "crpNPCCorporations" n ON n."corporationID" = e."corporationID"
                WHERE e."characterID" = $1 ORDER BY e."startDate" DESC LIMIT 1"#,
            &[&(me as i32)],
        )
        .await
        .map_err(|e| db_failed("finding the NPC corporation", e))?
        .and_then(|r| int(&r, 0))
        .ok_or_else(|| Outcome::custom("There is no NPC corporation for you to go back to."))?;
    crate::svc::membership::change_corporation(&tx, me, home, 0, crate::bulkdata::filetime_now()).await?;
    tx.execute(r#"UPDATE "corporation" SET "deleted" = 1, "ceoID" = 0 WHERE "corporationID" = $1"#, &[&(corp as i32)])
        .await
        .map_err(|e| db_failed("closing the corporation", e))?;
    tx.commit().await.map_err(|e| db_failed("COMMIT", e))?;
    crate::svc::membership::member_moved(ctx.world, me, corp, home);
    info!("{me} resigned as the last member of corporation {corp}, which is closed");
    ctx.world.lock().reaffiliate(&[], &[me]);
    Ok(())
}

/// Votes past their end: the option with the most shares behind it wins (the
/// first, on a tie) and becomes a sanctioned action. Run by the heartbeat.
pub async fn close_votes(world: &World, db: &Client) {
    let now = crate::bulkdata::filetime_now();
    let sql = r#"SELECT c."voteCaseID", c."corporationID", c."endDateTime",
                        (SELECT o."optionID" FROM "crpVoteOptions" o
                          WHERE o."voteCaseID" = c."voteCaseID"
                          ORDER BY COALESCE((SELECT sum(sh."shares") FROM "crpVotes" v
                                               JOIN "crpCharShares" sh ON sh."characterID" = v."characterID" AND sh."corporationID" = c."corporationID"
                                              WHERE v."voteCaseID" = c."voteCaseID" AND v."optionID" = o."optionID"), 0) DESC,
                                   o."optionID"
                          LIMIT 1)
                   FROM "crpVoteCases" c
                  WHERE c."endDateTime" <= $1
                    AND NOT EXISTS (SELECT 1 FROM "crpSanctionedActions" s WHERE s."voteCaseID" = c."voteCaseID")"#;
    let closed = match db.query(sql, &[&now]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("reading the votes to close: {}", crate::logging::chain(&e));
            return;
        }
    };
    for r in closed {
        let (case, corp, end, option) = (int(&r, 0).unwrap_or(0), int(&r, 1).unwrap_or(0), int(&r, 2).unwrap_or(now), int(&r, 3));
        let saved = db
            .execute(
                r#"INSERT INTO "crpSanctionedActions" ("voteCaseID", "corporationID", "timeActioned", "actedUpon", "inEffect", "optionID", "expires")
                   VALUES ($1, $2, $3, 0, 0, $4, $5)"#,
                &[&(case as i32), &(corp as i32), &now, &option.map(|o| o as i32), &(end + ACT_WITHIN)],
            )
            .await;
        if let Err(e) = saved {
            error!("closing vote {case}: {}", crate::logging::chain(&e));
            continue;
        }
        info!("vote {case} of corporation {corp} closed: option {option:?}");
        elect(world, db, case, corp).await;
        let sql = format!(r#"{SANCTION_SELECT} WHERE s."voteCaseID" = $1"#);
        if let Ok(Some(row)) = db.query_opt(sql.as_str(), &[&(case as i32)]).await {
            let new = line(&row);
            tell(world, corp, "OnSanctionedActionChanged", vec![Value::Int(corp), Value::Int(case), change(SANCTION_HEADER, None, Some(&new))]);
        }
    }
}

/// A CEO vote the candidate won takes effect as it closes.
async fn elect(world: &World, db: &Client, case: i64, corp: i64) {
    let sql = r#"SELECT o."parameter", co."ceoID"
                   FROM "crpSanctionedActions" s
                   JOIN "crpVoteCases" c ON c."voteCaseID" = s."voteCaseID"
                   JOIN "crpVoteOptions" o ON o."voteCaseID" = s."voteCaseID" AND o."optionID" = s."optionID"
                   JOIN "corporation" co ON co."corporationID" = c."corporationID"
                  WHERE s."voteCaseID" = $1 AND c."voteType" = $2"#;
    let Ok(Some(r)) = db.query_opt(sql, &[&(case as i32), &(VOTE_CEO as i16)]).await else { return };
    let (Some(winner), old) = (int(&r, 0).filter(|w| *w != 0), int(&r, 1).unwrap_or(0)) else { return };
    let member = db
        .query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID" = $1 AND "corporationID" = $2"#, &[&(winner as i32), &(corp as i32)])
        .await
        .ok()
        .flatten()
        .is_some();
    if !member || winner == old {
        return;
    }
    if hand_over(db, world, corp, old, winner).await.is_ok() {
        let now = crate::bulkdata::filetime_now();
        let done = r#"UPDATE "crpSanctionedActions" SET "actedUpon" = 1, "inEffect" = 1, "timeActedUpon" = $2 WHERE "voteCaseID" = $1"#;
        if let Err(e) = db.execute(done, &[&(case as i32), &now]).await {
            error!("recording the election {case}: {}", crate::logging::chain(&e));
        }
    }
}

/// Whether an item is locked down by its corporation's vote.
pub async fn locked(db: &impl GenericClient, item: i64) -> bool {
    let sql = r#"SELECT 1 FROM "crpItemLocks" WHERE "itemID" = $1"#;
    db.query_opt(sql, &[&(item as i32)]).await.ok().flatten().is_some()
}

/// The stations where a corporation has items locked down.
pub async fn locked_locations(ctx: &Ctx<'_>) -> Value {
    let corp = ctx.session.corp_id.unwrap_or(0) as i32;
    let sql = r#"SELECT DISTINCT "locationID" FROM "crpItemLocks" WHERE "corporationID" = $1 AND "locationID" IS NOT NULL"#;
    Value::List(ctx.query(sql, &[&corp]).await.iter().filter_map(|r| int(r, 0)).map(Value::Int).collect())
}

fn tell(world: &World, corp: i64, event: &'static str, args: Vec<Value>) {
    let w = world.lock();
    w.notify(&w.corp_members(corp), event, Value::Tuple(args));
}
