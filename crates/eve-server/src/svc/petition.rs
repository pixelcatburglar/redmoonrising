//! `petitioner`: the help window's petitions (ui/shared/neocom/petition.py).
//!
//! A pilot files a petition in a category; it waits in its category's queue
//! until a petitionee (a session with ROLE_PETITIONEE) claims it. The two
//! then trade messages, each side hearing OnPetitionMessage as the other
//! writes; a petitionee may add comments, which the pilot never sees. The
//! petitionee closes it, hands it back, or escalates it to the queue the
//! category names. Every step is written to the petition's log.
//!
//! The categories, their extra fields and the queues are server data the
//! client only reads; db/petitionCategories.sql.gz and its neighbours hold a
//! set made for this server. Categories 1 and -5 open a web page in the
//! client and are left out; 4 and 5 carry the pilot's chat logs, and 7 no
//! combat log.

use std::collections::HashMap;

use eve_marshal::Value;
use tracing::info;

use crate::services::{Call, Ctx, Outcome};
use crate::session::ROLE_PETITIONEE;
use crate::svc::membership::{message, Refused};
use crate::svc::rows::{int, line, row, rowset, text};

/// const.maxPetitionsPerDay.
const MAX_PER_DAY: i64 = 8;
const DAY: i64 = 24 * 3600 * 10_000_000;
const TEXT_MAX: usize = 1_000;
/// const.petitionMaxChatLogSize and petitionMaxCombatLogSize.
const LOG_MAX: usize = 200_000;

/// What the log records, as petitionLog.actionID, with the text it shows.
#[derive(Clone, Copy)]
enum Action {
    Created = 1,
    Claimed,
    Unclaimed,
    Escalated,
    Closed,
    Cancelled,
    Deleted,
    Message,
    Comment,
}

impl Action {
    fn text(self) -> &'static str {
        match self {
            Action::Created => "Petition created",
            Action::Claimed => "Petition claimed",
            Action::Unclaimed => "Petition unclaimed",
            Action::Escalated => "Petition escalated",
            Action::Closed => "Petition closed",
            Action::Cancelled => "Petition cancelled",
            Action::Deleted => "Petition deleted",
            Action::Message => "Message sent",
            Action::Comment => "Comment added",
        }
    }
}

/// The petition as the list entries and the view window read it
/// (PetitionField.Load and ShowPetition). touchDate is the last message, or
/// the filing; updated is 1 when the petitionee spoke last, which the queue
/// shows as "Waiting".
const PETITION_HEADER: &[&str] = &[
    "petitionID", "petitionerID", "categoryID", "queueID", "subject", "petition", "createDate",
    "touchDate", "claimed", "claimedBy", "closed", "closedAt", "deleted", "escalatesTo", "updated",
    "isOnline", "properties",
];
const PETITION_SELECT: &str = r#"SELECT p."petitionID", p."petitionerID", p."categoryID", p."queueID", p."subject",
        p."petition", p."createdAt",
        COALESCE((SELECT max(m."created") FROM "petitionMessages" m WHERE m."petitionID" = p."petitionID"), p."createdAt"),
        p."claimed", p."claimedBy", p."closed", p."closedAt", p."deleted", p."escalatesTo",
        COALESCE((SELECT m."isPetitionee" FROM "petitionMessages" m
                   WHERE m."petitionID" = p."petitionID" AND m."comment" = 0
                   ORDER BY m."messageID" DESC LIMIT 1), 0)
   FROM "petitions" p"#;
const MESSAGE_HEADER: &[&str] = &["messageID", "petitionID", "senderID", "text", "sentDate", "comment", "read"];
const LOG_HEADER: &[&str] = &["logID", "petitionID", "logDateTime", "characterID", "eventText", "userName"];

pub async fn petitioner(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let me = ctx.session.char_id.unwrap_or(0);
    let id = call.arg_id(0);
    let result = match call.method.as_str() {
        // CheckNewMessages reads .petitionID, .text and .messageID.
        "GetUnreadMessages" => {
            let sql = r#"SELECT m."messageID", m."petitionID", m."text" FROM "petitionMessages" m
                           JOIN "petitions" p ON p."petitionID" = m."petitionID"
                          WHERE p."petitionerID" = $1 AND m."isPetitionee" = 1 AND m."comment" = 0 AND m."read" = 0
                          ORDER BY m."messageID""#;
            Ok(ctx.rowset(&["messageID", "petitionID", "text"], sql, &[&(me as i32)]).await)
        }
        "MarkAsRead" => {
            let sql = r#"UPDATE "petitionMessages" m SET "read" = 1 FROM "petitions" p
                          WHERE m."messageID" = $1 AND p."petitionID" = m."petitionID"
                            AND (p."petitionerID" = $2 OR p."claimedBy" = $2)"#;
            ctx.execute(sql, &[&(id as i32), &(me as i32)]).await;
            Ok(Value::None)
        }
        "GetMyPetitions" => Ok(petitions(ctx, r#"WHERE p."petitionerID" = $1 AND p."deleted" = 0"#, me).await),
        "GetClaimedPetitions" => match petitionee(ctx) {
            Ok(()) => Ok(petitions(ctx, r#"WHERE p."claimedBy" = $1 AND p."claimed" = 1 AND p."closed" = 0"#, me).await),
            Err(e) => Err(e),
        },
        "GetPetitionQueue" => match petitionee(ctx) {
            Ok(()) => Ok(petitions(ctx, r#"WHERE p."queueID" = $1 AND p."claimed" = 0 AND p."closed" = 0 AND p."deleted" = 0"#, id).await),
            Err(e) => Err(e),
        },
        "GetPetition" => get_petition(ctx, id).await,
        "GetPetitionMessages" => messages(ctx, id).await,
        "GetPetitionLog" => log_of(ctx, id).await,
        "GetCategories" => {
            let sql = r#"SELECT "categoryID","displayName","description","escalatesTo"
                           FROM "petitionCategories" ORDER BY "categoryID""#;
            Ok(ctx.rowset(&["categoryID", "displayName", "description", "escalatesTo"], sql, &[]).await)
        }
        "GetCategoryName" => {
            let sql = r#"SELECT "displayName" FROM "petitionCategories" WHERE "categoryID" = $1"#;
            let name = ctx.query_one(sql, &[&(id as i32)]).await.and_then(|r| text(&r, 0));
            Ok(Value::text(name.unwrap_or_default()))
        }
        "GetCategoryProperties" => {
            let sql = r#"SELECT "k" FROM "petitionCategoryProperties" WHERE "categoryID" = $1 ORDER BY "orderID""#;
            Ok(ctx.rowset(&["k"], sql, &[&(id as i32)]).await)
        }
        "GetQueues" => {
            let sql = r#"SELECT "queueID","queueName" FROM "petitionQueues" ORDER BY "queueID""#;
            Ok(ctx.rowset(&["queueID", "queueName"], sql, &[]).await)
        }
        "MayPetition" => Ok(Value::Int(may_petition(ctx, id, call.arg_i64(1)).await)),
        "CreatePetition" => create(ctx, call).await,
        "PetitionerChat" => chat(ctx, id, &call.arg_text(1).unwrap_or_default(), false, false).await,
        "PetitioneeChat" => {
            let comment = call.arg_i64(2).unwrap_or(0) != 0;
            chat(ctx, id, &call.arg_text(1).unwrap_or_default(), true, comment).await
        }
        "ClaimPetition" => claim(ctx, id).await,
        "UnClaimPetition" => unclaim(ctx, id, None).await,
        "EscalatePetition" => unclaim(ctx, id, Some(call.arg_i64(1))).await,
        "ClosePetition" => close(ctx, id).await,
        "CancelPetition" => withdraw(ctx, id, Action::Cancelled).await,
        "DeletePetition" => withdraw(ctx, id, Action::Deleted).await,
        _ => return None,
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

fn petitionee(ctx: &Ctx<'_>) -> Refused<()> {
    if ctx.session.role & ROLE_PETITIONEE == ROLE_PETITIONEE {
        Ok(())
    } else {
        Err(Outcome::custom("Only petitionees may do that."))
    }
}

/// Petitions matching `filter` (which takes one parameter), each with whether
/// its pilot is online and its extra fields.
async fn petitions(ctx: &Ctx<'_>, filter: &str, param: i64) -> Value {
    let sql = format!(r#"{PETITION_SELECT} {filter} ORDER BY p."petitionID""#);
    let rows = ctx.query(&sql, &[&(param as i32)]).await;
    let ids: Vec<i32> = rows.iter().filter_map(|r| int(r, 0)).map(|i| i as i32).collect();
    let mut properties: HashMap<i64, Vec<(Value, Value)>> = HashMap::new();
    let sql = r#"SELECT "petitionID", "k", "v" FROM "petitionProperties" WHERE "petitionID" = ANY($1) ORDER BY "k""#;
    for r in ctx.query(sql, &[&ids]).await {
        let (k, v) = (text(&r, 1).unwrap_or_default(), text(&r, 2).unwrap_or_default());
        properties.entry(int(&r, 0).unwrap_or(0)).or_default().push((Value::text(k), Value::text(v)));
    }
    let world = ctx.world.lock();
    let lines = rows
        .iter()
        .map(|r| {
            let mut l = line(r);
            let id = int(r, 0).unwrap_or(0);
            l.push(Value::Int(i64::from(world.is_online(int(r, 1).unwrap_or(0)))));
            l.push(properties.remove(&id).map_or(Value::None, Value::Dict));
            Value::List(l)
        })
        .collect();
    rowset(PETITION_HEADER, lines)
}

/// A petition's petitioner and claimant, if the caller may see it: its
/// petitioner, or a petitionee.
async fn parties(ctx: &Ctx<'_>, id: i64) -> Refused<Parties> {
    let sql = r#"SELECT "petitionerID", "claimedBy", "claimed", "closed", "escalatesTo" FROM "petitions"
                  WHERE "petitionID" = $1 AND "deleted" = 0"#;
    let r = ctx.query_one(sql, &[&(id as i32)]).await.ok_or_else(|| Outcome::custom("That petition does not exist."))?;
    let p = Parties {
        petitioner: int(&r, 0).unwrap_or(0),
        claimant: int(&r, 1).filter(|_| int(&r, 2) == Some(1)),
        closed: int(&r, 3) == Some(1),
        escalates_to: int(&r, 4),
    };
    if Some(p.petitioner) != ctx.session.char_id && petitionee(ctx).is_err() {
        return Err(Outcome::custom("That is not your petition."));
    }
    Ok(p)
}

struct Parties {
    petitioner: i64,
    claimant: Option<i64>,
    closed: bool,
    escalates_to: Option<i64>,
}

async fn get_petition(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    parties(ctx, id).await?;
    let found = petitions(ctx, r#"WHERE p."petitionID" = $1"#, id).await;
    let line = match &found {
        Value::Instance { state, .. } => match state.as_ref() {
            Value::Dict(d) => d.iter().find(|(k, _)| *k == Value::bytes("lines")).and_then(|(_, v)| v.seq()).and_then(|l| l.first().cloned()),
            _ => None,
        },
        _ => None,
    };
    Ok(match line {
        Some(Value::List(l)) => row(PETITION_HEADER, l),
        _ => Value::None,
    })
}

/// The messages; comments only for petitionees.
async fn messages(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    parties(ctx, id).await?;
    let comments = i16::from(petitionee(ctx).is_ok());
    let sql = r#"SELECT "messageID", "petitionID", "senderID", "text", "created", "comment", "read"
                   FROM "petitionMessages" WHERE "petitionID" = $1 AND "comment" <= $2 ORDER BY "messageID""#;
    Ok(ctx.rowset(MESSAGE_HEADER, sql, &[&(id as i32), &comments]).await)
}

async fn log_of(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    petitionee(ctx)?;
    let sql = r#"SELECT l."logID", l."petitionID", l."logDateTime", l."actorID", COALESCE(l."message", ''),
                        COALESCE(a."accountName", '')
                   FROM "petitionLog" l
                   LEFT JOIN "character_" c ON c."characterID" = l."actorID"
                   LEFT JOIN "account" a ON a."accountID" = c."accountID"
                  WHERE l."petitionID" = $1 ORDER BY l."logID""#;
    Ok(ctx.rowset(LOG_HEADER, sql, &[&(id as i32)]).await)
}

/// MayPetition(categoryID, OocCharacterID): 1, or the client's refusals:
/// -1 one open petition per category, -2 too many today, -3 no character,
/// -4 not one of the account's characters.
async fn may_petition(ctx: &Ctx<'_>, category: i64, ooc: Option<i64>) -> i64 {
    let Some(char_id) = petitioner_of(ctx, ooc).await else {
        return if ctx.session.char_id.is_none() && ooc.is_none() { -3 } else { -4 };
    };
    let open = r#"SELECT 1 FROM "petitions" WHERE "petitionerID" = $1 AND "categoryID" = $2 AND "closed" = 0 AND "deleted" = 0"#;
    if ctx.query_one(open, &[&(char_id as i32), &(category as i32)]).await.is_some() {
        return -1;
    }
    let today = r#"SELECT count(*) FROM "petitions" WHERE "petitionerID" = $1 AND "createdAt" > $2"#;
    let since = crate::bulkdata::filetime_now() - DAY;
    let count = ctx.query_one(today, &[&(char_id as i32), &since]).await.and_then(|r| int(&r, 0)).unwrap_or(0);
    if count >= MAX_PER_DAY {
        return -2;
    }
    1
}

/// The character a petition is filed for: the session's, or (filed from
/// character selection) one of the account's.
async fn petitioner_of(ctx: &Ctx<'_>, ooc: Option<i64>) -> Option<i64> {
    if let Some(char_id) = ctx.session.char_id {
        return Some(char_id);
    }
    let ooc = ooc?;
    let sql = r#"SELECT 1 FROM "character_" WHERE "characterID" = $1 AND "accountID" = $2"#;
    ctx.query_one(sql, &[&(ooc as i32), &(ctx.session.user_id.unwrap_or(0) as i32)]).await.map(|_| ooc)
}

/// CreatePetition(subject, petition, categoryID, properties, OocCharacterID,
/// combatLog=None, chatLog=None): files it in its category's first queue.
async fn create(ctx: &Ctx<'_>, call: &Call) -> Refused<Value> {
    let category = call.arg_id(2);
    if may_petition(ctx, category, call.arg_i64(4)).await != 1 {
        return Ok(Value::Bool(false));
    }
    let Some(char_id) = petitioner_of(ctx, call.arg_i64(4)).await else { return Ok(Value::Bool(false)) };
    let cut = |v: Option<String>, n: usize| v.map(|s| s.chars().take(n).collect::<String>());
    let subject = cut(call.arg_text(0), 100).unwrap_or_default();
    let body = cut(call.arg_text(1), TEXT_MAX).unwrap_or_default();
    let log = |name: &str| call.kwarg(name).and_then(|v| v.as_string()).map(|s| s.chars().rev().take(LOG_MAX).collect::<Vec<_>>().into_iter().rev().collect::<String>());
    let (combat, chat) = (log("combatLog"), log("chatLog"));
    let sql = r#"INSERT INTO "petitions" ("petitionerID", "accountID", "categoryID", "queueID", "subject", "petition",
                                         "createdAt", "escalatesTo", "combatLog", "chatLog")
                 SELECT $1, $2, c."categoryID", (SELECT min("queueID") FROM "petitionQueues"), $4, $5, $6, c."escalatesTo", $7, $8
                   FROM "petitionCategories" c WHERE c."categoryID" = $3
                 RETURNING "petitionID""#;
    let now = crate::bulkdata::filetime_now();
    let account = ctx.session.user_id.unwrap_or(0) as i32;
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 8] =
        [&(char_id as i32), &account, &(category as i32), &subject, &body, &now, &combat, &chat];
    let Some(id) = ctx.query_one(sql, &params).await.and_then(|r| int(&r, 0)) else {
        return Err(Outcome::custom("That petition category does not exist."));
    };
    if let Some(Value::Dict(fields)) = call.args.get(3) {
        for (k, v) in fields {
            let (Some(k), Some(v)) = (k.as_string(), v.as_string()) else { continue };
            let sql = r#"INSERT INTO "petitionProperties" ("petitionID", "k", "v") VALUES ($1, $2, $3) ON CONFLICT DO NOTHING"#;
            ctx.execute(sql, &[&(id as i32), &k, &cut(Some(v), TEXT_MAX).unwrap_or_default()]).await;
        }
    }
    record(ctx, id, char_id, Action::Created).await;
    info!("{char_id} filed petition {id} in category {category}: {subject:?}");
    let w = ctx.world.lock();
    let event = Value::Tuple(vec![Value::Int(id), Value::Int(char_id), Value::text(&subject), Value::Int(category)]);
    w.notify(&w.with_role(ROLE_PETITIONEE), "OnNewPetition", event);
    Ok(Value::Bool(true))
}

/// A message from the pilot, or from the petitionee who has it (a comment
/// stays among petitionees). Both sides hear it, the writer included, so an
/// open view reloads.
async fn chat(ctx: &Ctx<'_>, id: i64, text_in: &str, from_petitionee: bool, comment: bool) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let p = parties(ctx, id).await?;
    if from_petitionee {
        petitionee(ctx)?;
    } else if p.petitioner != me {
        return Err(Outcome::custom("That is not your petition."));
    }
    if p.closed {
        return Err(message("PetitionClosed"));
    }
    let body: String = text_in.chars().take(TEXT_MAX).collect();
    let sql = r#"INSERT INTO "petitionMessages" ("petitionID", "senderID", "isPetitionee", "text", "created", "comment")
                 VALUES ($1, $2, $3, $4, $5, $6) RETURNING "messageID""#;
    let now = crate::bulkdata::filetime_now();
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 6] =
        [&(id as i32), &(me as i32), &i16::from(from_petitionee), &body, &now, &i16::from(comment)];
    let message_id = ctx.query_one(sql, &params).await.and_then(|r| int(&r, 0)).ok_or_else(|| Outcome::custom("The message could not be saved."))?;
    record(ctx, id, me, if comment { Action::Comment } else { Action::Message }).await;
    let mut to = vec![me];
    to.extend(p.claimant);
    if !comment {
        to.push(p.petitioner);
    }
    tell(ctx, &to, "OnPetitionMessage", vec![Value::Int(id), Value::Int(me), Value::text(&body), Value::Int(message_id)]);
    Ok(Value::None)
}

/// ClaimPetition(petitionID): 1 if the caller now has it, 0 if someone else
/// got there first.
async fn claim(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    petitionee(ctx)?;
    let me = ctx.session.char_id.unwrap_or(0);
    let sql = r#"UPDATE "petitions" SET "claimed" = 1, "claimedBy" = $2
                  WHERE "petitionID" = $1 AND "claimed" = 0 AND "closed" = 0 AND "deleted" = 0 AND "petitionerID" <> $2
                 RETURNING "petitionerID""#;
    let Some(petitioner) = ctx.query_one(sql, &[&(id as i32), &(me as i32)]).await.and_then(|r| int(&r, 0)) else {
        return Ok(Value::Int(0));
    };
    record(ctx, id, me, Action::Claimed).await;
    info!("{me} claimed petition {id}");
    tell(ctx, &[petitioner], "OnAssignPetition", vec![Value::Int(id), Value::Int(me)]);
    Ok(Value::Int(1))
}

/// UnClaimPetition(petitionID) puts it back in its queue;
/// EscalatePetition(petitionID, escalatesTo) into the queue above.
async fn unclaim(ctx: &Ctx<'_>, id: i64, escalate: Option<Option<i64>>) -> Refused<Value> {
    petitionee(ctx)?;
    let me = ctx.session.char_id.unwrap_or(0);
    let p = parties(ctx, id).await?;
    if p.claimant != Some(me) {
        return Err(Outcome::custom("You have not claimed that petition."));
    }
    let queue = match escalate {
        None => None,
        Some(to) => Some(to.or(p.escalates_to).ok_or_else(|| Outcome::custom("That petition cannot be escalated any further."))?),
    };
    let sql = r#"UPDATE "petitions" SET "claimed" = 0, "claimedBy" = NULL,
                        "queueID" = COALESCE($2, "queueID"),
                        "escalatesTo" = CASE WHEN $2::int IS NULL THEN "escalatesTo" ELSE NULL END
                  WHERE "petitionID" = $1"#;
    ctx.execute(sql, &[&(id as i32), &queue.map(|q| q as i32)]).await;
    record(ctx, id, me, if queue.is_some() { Action::Escalated } else { Action::Unclaimed }).await;
    info!("{me} {} petition {id}", if queue.is_some() { "escalated" } else { "unclaimed" });
    tell(ctx, &[p.petitioner], "OnAssignPetition", vec![Value::Int(id), Value::None]);
    Ok(Value::None)
}

async fn close(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    petitionee(ctx)?;
    let me = ctx.session.char_id.unwrap_or(0);
    let p = parties(ctx, id).await?;
    let sql = r#"UPDATE "petitions" SET "closed" = 1, "closedAt" = $2 WHERE "petitionID" = $1 AND "closed" = 0"#;
    ctx.execute(sql, &[&(id as i32), &crate::bulkdata::filetime_now()]).await;
    record(ctx, id, me, Action::Closed).await;
    info!("{me} closed petition {id}");
    tell(ctx, &[p.petitioner], "OnClosePetition", vec![Value::Int(id)]);
    Ok(Value::None)
}

/// The pilot takes back a petition nobody has claimed (CancelPetition), or
/// clears away a closed one (DeletePetition).
async fn withdraw(ctx: &Ctx<'_>, id: i64, how: Action) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let p = parties(ctx, id).await?;
    if p.petitioner != me {
        return Err(Outcome::custom("That is not your petition."));
    }
    let sql = match how {
        Action::Cancelled => r#"UPDATE "petitions" SET "deleted" = 1, "closed" = 1, "closedAt" = $2 WHERE "petitionID" = $1 AND "claimed" = 0"#,
        _ => r#"UPDATE "petitions" SET "deleted" = 1, "closedAt" = COALESCE("closedAt", $2) WHERE "petitionID" = $1 AND "closed" = 1"#,
    };
    if ctx.db.execute(sql, &[&(id as i32), &crate::bulkdata::filetime_now()]).await.unwrap_or(0) == 0 {
        return Err(Outcome::custom(match how {
            Action::Cancelled => "A petition someone has claimed cannot be cancelled.",
            _ => "Only a closed petition can be deleted.",
        }));
    }
    record(ctx, id, me, how).await;
    info!("{me}: {} ({id})", how.text());
    Ok(Value::None)
}

async fn record(ctx: &Ctx<'_>, id: i64, actor: i64, what: Action) {
    let sql = r#"INSERT INTO "petitionLog" ("petitionID", "logDateTime", "actorID", "actionID", "message") VALUES ($1, $2, $3, $4, $5)"#;
    let now = crate::bulkdata::filetime_now();
    ctx.execute(sql, &[&(id as i32), &now, &(actor as i32), &(what as i32), &what.text()]).await;
}

fn tell(ctx: &Ctx<'_>, chars: &[i64], event: &'static str, args: Vec<Value>) {
    let w = ctx.world.lock();
    let mut clients: Vec<i64> = chars.iter().flat_map(|c| w.clients_of(*c)).collect();
    clients.sort_unstable();
    clients.dedup();
    w.notify(&clients, event, Value::Tuple(args));
}
