use eve_marshal::Value;
use tokio_postgres::Row as PgRow;
use tracing::{error, warn};

use crate::services::{Call, Ctx, Outcome};
use crate::session::Session;
use crate::svc::rows::{int, row, rowset, text};
use crate::world::WorldState;

const OWNER_SYSTEM: i32 = 1;

const CHTMODE_LISTENER: i64 = 1;
const CHTMODE_CONVERSATIONALIST: i64 = 3;
const CHTMODE_CREATOR: i64 = 15;
const CHTMODE_NOTSPECIFIED: i64 = -1;

/// SQL true when $2 may run channel $1: its owner, or listed as an operator
/// (CHTMODE_OPERATOR = 7).
const OPERATOR_TEST: &str = r#""ownerID" = $2 OR EXISTS (
    SELECT 1 FROM "channelMods" m WHERE m."channelID" = $1 AND m."accessor" = $2 AND m."mode" >= 7)"#;

const CSPA_MAILBOX: i64 = 127;

const CHANNEL_HEADER: &[&str] = &[
    "channelID", "ownerID", "displayName", "motd", "comparisonKey", "memberless",
    "password", "mailingList", "cspa", "temporary", "mode", "subscribed",
    "estimatedMemberCount",
];

const IX_SUBSCRIBED: usize = 11;

const ACL_HEADER: &[&str] =
    &["accessor", "mode", "untilWhen", "reason", "admin", "originalMode"];

const MEMBER_HEADER: &[&str] = &["charID", "corpID", "allianceID", "role", "extra"];

pub async fn lsc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "JoinChannels" => join_channels(ctx, call.args.first()).await,
        "GetChannels" => get_channels(ctx).await,
        "GetMembers" => get_members(ctx, call.args.first()).await,
        // LeaveChannels(channelIDs, unsubscribe) / LeaveChannel(channelID, unsubscribe)
        "LeaveChannels" => {
            for channel in call.args.first().and_then(|v| v.seq()).unwrap_or(&[]) {
                leave(ctx, channel);
            }
            Value::None
        }
        "LeaveChannel" => {
            if let Some(channel) = call.args.first() {
                leave(ctx, channel);
            }
            Value::None
        }
        // SendMessage(channelID, message): everyone with the channel open hears it,
        // the speaker included (lscengine only shows a line when OnLSC brings it).
        "SendMessage" => {
            if let (Some(channel), Some(message)) = (call.args.first(), call.args.get(1)) {
                let world = ctx.world.lock();
                let members = world.channel_members(channel);
                if members.contains(&ctx.client_id) {
                    announce(&world, &members, channel, "SendMessage", &who(ctx.session), vec![message.clone()]);
                } else {
                    warn!("SendMessage to a channel the speaker has not joined");
                }
            }
            Value::None
        }
        "GetMyMessages" => get_my_messages(ctx).await,
        "MarkMessagesRead" => mark_messages_read(ctx, call.args.first()).await,
        "DeleteMessages" => delete_messages(ctx, call.arg_id(0), call.args.get(1)).await,
        "CreateChannel" => create_channel(ctx, call).await,
        "Configure" => configure(ctx, call).await,
        "RenameChannel" => {
            let sql = r#"UPDATE "channels" SET "displayName" = $3, "comparisonKey" = lower($3)
                          WHERE "channelID" = $1 AND "ownerID" = $2 RETURNING 1"#;
            let name = call.arg_text(1).unwrap_or_default();
            if ctx.query_one(sql, &[&(call.arg_id(0) as i32), &me(ctx), &name]).await.is_some() {
                announce_change(ctx, call, "RenameChannel", vec![Value::text(name)]);
            }
            Value::None
        }
        "SetChannelMOTD" => {
            let sql = format!(
                r#"UPDATE "channels" SET "motd" = $3 WHERE "channelID" = $1 AND ({OPERATOR_TEST}) RETURNING 1"#
            );
            let motd = call.arg_text(1).unwrap_or_default();
            if ctx.query_one(&sql, &[&(call.arg_id(0) as i32), &me(ctx), &motd]).await.is_some() {
                announce_change(ctx, call, "SetChannelMOTD", vec![Value::text(motd)]);
            }
            Value::None
        }
        "DestroyChannel" => {
            let sql = r#"WITH target AS (SELECT "channelID" FROM "channels"
                                          WHERE "channelID" = $1 AND "ownerID" = $2),
                              members AS (DELETE FROM "channelChars" WHERE "channelID" IN (SELECT * FROM target)),
                              mods AS (DELETE FROM "channelMods" WHERE "channelID" IN (SELECT * FROM target))
                         DELETE FROM "channels" WHERE "channelID" IN (SELECT * FROM target)
                         RETURNING 1"#;
            if ctx.query_one(sql, &[&(call.arg_id(0) as i32), &me(ctx)]).await.is_some() {
                announce_change(ctx, call, "DestroyChannel", Vec::new());
            }
            Value::None
        }
        "ForgetChannel" => {
            let sql = r#"DELETE FROM "channelChars" WHERE "channelID" = $1 AND "charID" = $2"#;
            ctx.execute(sql, &[&(call.arg_id(0) as i32), &me(ctx)]).await;
            Value::None
        }
        // (channelID, accessor, mode, untilWhen=None, reason=None). On the caller's
        // own mailbox (channelID = charID) this is how contacts are blocked.
        "AccessControl" => access_control(ctx, call).await,

        "GetMessageDetails" | "Page" => {
            return Some(Outcome::Unknown(format!(
                "LSC::{} needs the evemailDetails body/attachment path (recon/10 §5.2-§5.3)",
                call.method
            )))
        }

        _ => return None,
    };
    Some(Outcome::Value(value))
}

async fn get_my_messages(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] =
        &["channelID", "messageID", "senderID", "subject", "created", "read"];
    let Some(char_id) = ctx.session.char_id else { return rowset(HEADER, Vec::new()) };
    let mailboxes: Vec<i32> =
        own_entity_ids(ctx).into_iter().filter_map(|id| i32::try_from(id).ok()).collect();

    let sql = r#"SELECT "channelID","messageID","senderID","subject","created","read"
                   FROM "evemail"
                  WHERE "channelID" = ANY($1)
                     OR "channelID" IN (SELECT cc."channelID"
                                          FROM "channelChars" cc
                                          JOIN "channels" c ON c."channelID" = cc."channelID"
                                         WHERE cc."charID" = $2 AND c."mailingList" <> 0)
                  ORDER BY "created" DESC, "messageID" DESC"#;
    match ctx.db.query(sql, &[&mailboxes, &(char_id as i32)]).await {
        Ok(rows) => {
            rowset(HEADER, rows.iter().map(|r| Value::List(crate::svc::rows::line(r))).collect())
        }
        Err(e) => {
            error!("LSC.GetMyMessages({char_id}): {err}", err = crate::logging::chain(&e));
            rowset(HEADER, Vec::new())
        }
    }
}

async fn mark_messages_read(ctx: &Ctx<'_>, arg: Option<&Value>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let ids = id_list(arg);
    if ids.is_empty() {
        return Value::None;
    }
    let mailboxes: Vec<i32> =
        own_entity_ids(ctx).into_iter().filter_map(|id| i32::try_from(id).ok()).collect();
    let sql = r#"UPDATE "evemail" SET "read" = 1
                  WHERE "messageID" = ANY($1)
                    AND ("channelID" = ANY($2)
                         OR "channelID" IN (SELECT cc."channelID"
                                              FROM "channelChars" cc
                                              JOIN "channels" c ON c."channelID" = cc."channelID"
                                             WHERE cc."charID" = $3 AND c."mailingList" <> 0))"#;
    if let Err(e) = ctx.db.execute(sql, &[&ids, &mailboxes, &(char_id as i32)]).await {
        error!("LSC.MarkMessagesRead({} ids): {err}", ids.len(), err = crate::logging::chain(&e));
    }
    Value::None
}

async fn delete_messages(ctx: &Ctx<'_>, channel_id: i64, arg: Option<&Value>) -> Value {
    let ids = id_list(arg);
    if ids.is_empty() {
        return Value::None;
    }
    let Ok(channel) = i32::try_from(channel_id) else { return Value::None };
    if !own_entity_ids(ctx).contains(&channel_id) {
        warn!("LSC.DeleteMessages: channel {channel_id} is not one of the caller's");
        return Value::None;
    }
    let sql = r#"DELETE FROM "evemail" WHERE "messageID" = ANY($1) AND "channelID" = $2"#;
    if let Err(e) = ctx.db.execute(sql, &[&ids, &channel]).await {
        error!("LSC.DeleteMessages({channel_id}): {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

fn id_list(arg: Option<&Value>) -> Vec<i32> {
    arg.and_then(|v| v.seq())
        .map(|s| s.iter().filter_map(|v| v.as_i64()).filter_map(|v| i32::try_from(v).ok()).collect())
        .unwrap_or_default()
}

async fn join_channels(ctx: &Ctx<'_>, arg: Option<&Value>) -> Value {
    let requested: Vec<Value> =
        arg.and_then(|v| v.seq()).map(|s| s.to_vec()).unwrap_or_default();
    let me = self_member(ctx).await;
    let mut out = Vec::with_capacity(requested.len());
    for channel_id in &requested {
        let result = join_one(ctx, channel_id, me.as_ref()).await;
        let ok = matches!(&result, Value::Tuple(t) if t.get(1) == Some(&Value::Int(1)));
        if ok {
            let mut world = ctx.world.lock();
            let members = world.join_channel(channel_id, ctx.client_id);
            let others: Vec<i64> = members.iter().copied().filter(|c| *c != ctx.client_id).collect();
            let who = who(ctx.session);
            announce_counted(&world, &others, members.len(), channel_id, "JoinChannel", &who, Vec::new());
        }
        out.push(result);
    }
    Value::List(out)
}

fn leave(ctx: &Ctx<'_>, channel: &Value) {
    let mut world = ctx.world.lock();
    let members = world.leave_channel(channel, ctx.client_id);
    announce(&world, &members, channel, "LeaveChannel", &who(ctx.session), Vec::new());
}

/// Tells everyone with the call's channel (argument 0) open about a change to it.
fn announce_change(ctx: &Ctx<'_>, call: &Call, method: &'static str, args: Vec<Value>) {
    let Some(channel) = call.args.first() else { return };
    let world = ctx.world.lock();
    let members = world.channel_members(channel);
    announce(&world, &members, channel, method, &who(ctx.session), args);
}

/// The speaker as OnLSC wants it: (allianceID, corpID, [charID, name, typeID],
/// role, corpRole). The list form lets the client name the speaker without a
/// lookup (lscengine.OnLSC hints cfg.eveowners with it).
pub fn who(session: &Session) -> Value {
    Value::Tuple(vec![
        Value::int_or_none(session.alliance_id),
        Value::int_or_none(session.corp_id),
        Value::List(vec![
            Value::int_or_none(session.char_id),
            Value::text(session.char_name.clone()),
            Value::Int(session.char_type_id),
        ]),
        Value::Long(session.role),
        Value::Long(session.corp_role()),
    ])
}

/// OnLSC(channelID, estimatedMemberCount, method, who, args) to `members`.
pub fn announce(
    world: &WorldState,
    members: &[i64],
    channel: &Value,
    method: &'static str,
    who: &Value,
    args: Vec<Value>,
) {
    announce_counted(world, members, members.len(), channel, method, who, args);
}

fn announce_counted(
    world: &WorldState,
    targets: &[i64],
    member_count: usize,
    channel: &Value,
    method: &'static str,
    who: &Value,
    args: Vec<Value>,
) {
    let event = Value::Tuple(vec![
        channel.clone(),
        Value::Int(member_count as i64),
        Value::bytes(method),
        who.clone(),
        Value::Tuple(args),
    ]);
    world.notify(targets, "OnLSC", event);
}

async fn join_one(ctx: &Ctx<'_>, channel_id: &Value, me: Option<&Vec<Value>>) -> Value {
    let id_value = channel_id.clone();

    if let Some(id) = channel_id.as_i64() {
        return match channel_info(ctx, id).await {
            Some((line, default_mode)) => {
                let acl = acl_for(ctx, id, default_mode).await;
                let members = members_for(ctx, id, me).await;
                joined(id_value, row(CHANNEL_HEADER, line), acl, members)
            }
            None => refused(id_value, "That channel does not exist."),
        };
    }

    if channel_id.seq().is_some() {
        let lines = me.map(|m| vec![Value::List(m.clone())]).unwrap_or_default();
        return joined(
            id_value,
            Value::None,
            rowset(ACL_HEADER, Vec::new()),
            rowset(MEMBER_HEADER, lines),
        );
    }

    refused(id_value, "Malformed channel identifier.")
}

fn joined(channel_id: Value, info: Value, acl: Value, members: Value) -> Value {
    Value::Tuple(vec![
        channel_id,
        Value::Int(1),
        Value::Tuple(vec![info, acl, members]),
    ])
}

fn refused(channel_id: Value, why: &str) -> Value {
    Value::Tuple(vec![
        channel_id,
        Value::Int(0),
        Value::Tuple(vec![
            Value::bytes("LSCCannotJoin"),
            Value::Dict(vec![(Value::bytes("msg"), Value::bytes(why))]),
        ]),
    ])
}

async fn channel_info(ctx: &Ctx<'_>, channel_id: i64) -> Option<(Vec<Value>, i64)> {
    let sql = r#"SELECT "channelID","ownerID","displayName","motd","comparisonKey",
                        "memberless","password","mailingList","cspa","temporary",
                        "mode","subscribed","estimatedMemberCount"
                   FROM "channels" WHERE "channelID" = $1"#;
    match ctx.db.query(sql, &[&(channel_id as i32)]).await {
        Ok(rows) if !rows.is_empty() => {
            let mode = int(&rows[0], 10).unwrap_or(CHTMODE_LISTENER);
            Some((channel_line(&rows[0], false), mode))
        }
        Ok(_) => {
            if own_entity_ids(ctx).contains(&channel_id) {
                Some((synthetic_mailbox(channel_id), CHTMODE_LISTENER))
            } else {
                None
            }
        }
        Err(e) => {
            error!("LSC channel {channel_id}: {err}", err = crate::logging::chain(&e));
            None
        }
    }
}

fn channel_line(r: &PgRow, hide_password: bool) -> Vec<Value> {
    vec![
        Value::int_or_none(int(r, 0)),
        Value::int_or_none(int(r, 1)),
        text(r, 2).map_or(Value::None, Value::text),
        text(r, 3).map_or(Value::None, Value::text),
        text(r, 4).map_or(Value::None, Value::text),
        Value::int_or_none(int(r, 5)),
        if hide_password { Value::None } else { text(r, 6).map_or(Value::None, Value::text) },
        Value::int_or_none(int(r, 7)),
        Value::int_or_none(int(r, 8)),
        Value::int_or_none(int(r, 9)),
        Value::int_or_none(int(r, 10)),
        Value::int_or_none(int(r, 11)),
        Value::int_or_none(int(r, 12)),
    ]
}

fn synthetic_mailbox(owner_id: i64) -> Vec<Value> {
    vec![
        Value::Int(owner_id),
        Value::Int(owner_id),
        Value::None,
        Value::None,
        Value::None,
        Value::Int(1),
        Value::None,
        Value::Int(1),
        Value::Int(CSPA_MAILBOX),
        Value::Int(0),
        Value::Int(CHTMODE_LISTENER),
        Value::Int(1),
        Value::Int(1),
    ]
}

async fn get_channels(ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return rowset(CHANNEL_HEADER, Vec::new()) };
    let p_char = char_id as i32;
    let p_corp = ctx.session.corp_id.unwrap_or(-1) as i32;
    let p_alliance = ctx.session.alliance_id.unwrap_or(-1) as i32;

    let sql = r#"SELECT c."channelID",c."ownerID",c."displayName",c."motd",
                        c."comparisonKey",c."memberless",c."password",c."mailingList",
                        c."cspa",c."temporary",c."mode",c."subscribed",
                        c."estimatedMemberCount",
                        (cc."charID" IS NOT NULL) AS "isMember"
                   FROM "channels" c
                   LEFT JOIN "channelChars" cc
                          ON cc."channelID" = c."channelID" AND cc."charID" = $1
                  WHERE c."ownerID" IN ($1, $2, $3, $4) OR cc."charID" IS NOT NULL
                  ORDER BY c."channelID""#;

    let mut lines = Vec::new();
    let mut seen: Vec<i64> = Vec::new();
    match ctx.db.query(sql, &[&p_char, &p_corp, &p_alliance, &OWNER_SYSTEM]).await {
        Ok(rows) => {
            for r in &rows {
                let id = int(r, 0).unwrap_or(0);
                seen.push(id);
                let mut line = channel_line(r, true);
                let subscribed = (int(r, 13) == Some(1)) || own_entity_ids(ctx).contains(&id);
                line[IX_SUBSCRIBED] = Value::Int(subscribed as i64);
                lines.push(Value::List(line));
            }
        }
        Err(e) => error!("LSC GetChannels: {err}", err = crate::logging::chain(&e)),
    }

    for id in own_entity_ids(ctx) {
        if !seen.contains(&id) {
            lines.push(Value::List(synthetic_mailbox(id)));
        }
    }
    rowset(CHANNEL_HEADER, lines)
}

async fn get_members(ctx: &Ctx<'_>, arg: Option<&Value>) -> Value {
    let me = self_member(ctx).await;
    match arg.and_then(|v| v.as_i64()) {
        Some(id) => members_for(ctx, id, me.as_ref()).await,
        None => {
            let lines = me.map(|m| vec![Value::List(m)]).unwrap_or_default();
            rowset(MEMBER_HEADER, lines)
        }
    }
}

async fn members_for(ctx: &Ctx<'_>, channel_id: i64, me: Option<&Vec<Value>>) -> Value {
    let sql = r#"SELECT cc."charID",cc."corpID",cc."allianceID",cc."role",
                        ch."characterName",ch."typeID"
                   FROM "channelChars" cc
                   LEFT JOIN "character_" ch ON ch."characterID" = cc."charID"
                  WHERE cc."channelID" = $1
                  ORDER BY cc."charID""#;

    let my_id = ctx.session.char_id.unwrap_or(0);
    let mut lines = Vec::new();
    let mut have_self = false;
    match ctx.db.query(sql, &[&(channel_id as i32)]).await {
        Ok(rows) => {
            for r in &rows {
                let id = int(r, 0).unwrap_or(0);
                have_self |= id == my_id;
                lines.push(Value::List(vec![
                    Value::Int(id),
                    Value::int_or_none(nonzero(int(r, 1))),
                    Value::int_or_none(nonzero(int(r, 2))),
                    Value::Long(int(r, 3).unwrap_or(0)),
                    owner_hint(
                        id,
                        text(r, 4).unwrap_or_default(),
                        int(r, 5).unwrap_or(0),
                    ),
                ]));
            }
        }
        Err(e) => error!("LSC members of {channel_id}: {err}", err = crate::logging::chain(&e)),
    }
    if !have_self {
        if let Some(m) = me {
            lines.push(Value::List(m.clone()));
        }
    }
    rowset(MEMBER_HEADER, lines)
}

async fn self_member(ctx: &Ctx<'_>) -> Option<Vec<Value>> {
    let char_id = ctx.session.char_id?;
    let sql = r#"SELECT "characterName","typeID" FROM "character_"
                   WHERE "characterID" = $1"#;
    let (name, type_id) = match ctx.db.query(sql, &[&(char_id as i32)]).await {
        Ok(rows) if !rows.is_empty() => (
            text(&rows[0], 0).unwrap_or_default(),
            int(&rows[0], 1).unwrap_or(0),
        ),
        Ok(_) => (String::new(), 0),
        Err(e) => {
            error!("LSC self member {char_id}: {err}", err = crate::logging::chain(&e));
            (String::new(), 0)
        }
    };
    Some(vec![
        Value::Int(char_id),
        Value::int_or_none(ctx.session.corp_id),
        Value::int_or_none(ctx.session.alliance_id),
        Value::Long(ctx.session.role),
        owner_hint(char_id, name, type_id),
    ])
}

fn owner_hint(owner_id: i64, name: String, type_id: i64) -> Value {
    Value::List(vec![
        Value::Int(owner_id),
        Value::Bytes(name.into_bytes()),
        Value::Int(type_id),
    ])
}

async fn acl_for(ctx: &Ctx<'_>, channel_id: i64, default_mode: i64) -> Value {
    let sql = r#"SELECT "accessor","mode","untilWhen","reason","admin","originalMode"
                   FROM "channelMods" WHERE "channelID" = $1
                  ORDER BY "id""#;
    let mut lines = Vec::new();
    let mut has_default = false;
    match ctx.db.query(sql, &[&(channel_id as i32)]).await {
        Ok(rows) => {
            for r in &rows {
                let accessor = int(r, 0);
                has_default |= accessor.is_none();
                lines.push(Value::List(vec![
                    Value::int_or_none(accessor),
                    Value::int_or_none(int(r, 1)),
                    int(r, 2).map(Value::Long).unwrap_or(Value::None),
                    text(r, 3).map_or(Value::None, Value::text),
                    text(r, 4).map_or(Value::None, Value::text),
                    Value::int_or_none(int(r, 5)),
                ]));
            }
        }
        Err(e) => error!("LSC acl of {channel_id}: {err}", err = crate::logging::chain(&e)),
    }
    if !has_default {
        lines.insert(
            0,
            Value::List(vec![
                Value::None,
                Value::Int(default_mode),
                Value::None,
                Value::None,
                Value::None,
                Value::None,
            ]),
        );
    }
    rowset(ACL_HEADER, lines)
}

fn own_entity_ids(ctx: &Ctx<'_>) -> Vec<i64> {
    let mut ids = Vec::with_capacity(3);
    ids.extend(ctx.session.char_id);
    ids.extend(ctx.session.corp_id);
    ids.extend(ctx.session.alliance_id);
    ids
}

fn nonzero(v: Option<i64>) -> Option<i64> {
    v.filter(|n| *n != 0)
}

fn me(ctx: &Ctx<'_>) -> i32 {
    ctx.session.char_id.unwrap_or(0) as i32
}

fn flag(call: &Call, name: &str) -> i16 {
    i16::from(call.kwarg(name).and_then(|v| v.as_i64()).unwrap_or(0) != 0)
}

/// CreateChannel(displayName, joinExisting=1, memberless=0, mailingList=0,
/// temporary=False): lscengine unpacks (info, acl, memberList).
async fn create_channel(ctx: &Ctx<'_>, call: &Call) -> Value {
    let name = call.arg_text(0).unwrap_or_default();
    let char_id = me(ctx);
    let existing = if flag(call, "joinExisting") != 0 {
        let sql = r#"SELECT "channelID" FROM "channels"
                      WHERE "comparisonKey" = lower($1) AND "temporary" = 0 LIMIT 1"#;
        ctx.query_one(sql, &[&name]).await.and_then(|r| int(&r, 0))
    } else {
        None
    };
    let channel_id = match existing {
        Some(id) => id,
        None => {
            let sql = r#"INSERT INTO "channels" ("ownerID","displayName","motd","comparisonKey",
                                                 "memberless","mailingList","temporary","mode",
                                                 "estimatedMemberCount")
                         VALUES ($1, $2, '', lower($2), $3, $4, $5, $6, 1)
                         RETURNING "channelID""#;
            let mode = CHTMODE_CONVERSATIONALIST as i16;
            let params: [&(dyn tokio_postgres::types::ToSql + Sync); 6] = [
                &char_id,
                &name,
                &flag(call, "memberless"),
                &flag(call, "mailingList"),
                &flag(call, "temporary"),
                &mode,
            ];
            match ctx.query_one(sql, &params).await.and_then(|r| int(&r, 0)) {
                Some(id) => id,
                None => return Value::None,
            }
        }
    };
    let role = (if existing.is_some() { CHTMODE_CONVERSATIONALIST } else { CHTMODE_CREATOR }) as i32;
    let sql = r#"INSERT INTO "channelChars" ("channelID","charID","corpID","allianceID","role")
                 VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING"#;
    let corp = ctx.session.corp_id.unwrap_or(0) as i32;
    let alliance = ctx.session.alliance_id.unwrap_or(0) as i32;
    ctx.execute(sql, &[&(channel_id as i32), &char_id, &corp, &alliance, &role]).await;

    let Some((line, default_mode)) = channel_info(ctx, channel_id).await else { return Value::None };
    let acl = acl_for(ctx, channel_id, default_mode).await;
    let members = members_for(ctx, channel_id, self_member(ctx).await.as_ref()).await;
    Value::Tuple(vec![row(CHANNEL_HEADER, line), acl, members])
}

/// Configure(channelID, displayName=, motd=, cspa=, memberless=, oldPassword=,
/// newPassword=): the channel settings dialog. Owner only; the password changes
/// only if oldPassword matches.
async fn configure(ctx: &Ctx<'_>, call: &Call) -> Value {
    let channel_id = call.arg_id(0) as i32;
    let char_id = me(ctx);
    let text_kw = |name: &str| call.kwarg(name).and_then(|v| v.as_string());
    let int_kw = |name: &str| call.kwarg(name).and_then(|v| v.as_i64());
    if let Some(name) = text_kw("displayName") {
        let sql = r#"UPDATE "channels" SET "displayName" = $3, "comparisonKey" = lower($3)
                      WHERE "channelID" = $1 AND "ownerID" = $2"#;
        ctx.execute(sql, &[&channel_id, &char_id, &name]).await;
    }
    if let Some(motd) = text_kw("motd") {
        let sql = r#"UPDATE "channels" SET "motd" = $3 WHERE "channelID" = $1 AND "ownerID" = $2"#;
        ctx.execute(sql, &[&channel_id, &char_id, &motd]).await;
    }
    for column in ["cspa", "memberless"] {
        if let Some(v) = int_kw(column) {
            let sql = format!(
                r#"UPDATE "channels" SET "{column}" = $3 WHERE "channelID" = $1 AND "ownerID" = $2"#
            );
            ctx.execute(&sql, &[&channel_id, &char_id, &(v as i16)]).await;
        }
    }
    if call.kwarg("newPassword").is_some() {
        let new = text_kw("newPassword").filter(|p| !p.is_empty());
        let old = text_kw("oldPassword").unwrap_or_default();
        let sql = r#"UPDATE "channels" SET "password" = $3
                      WHERE "channelID" = $1 AND "ownerID" = $2 AND COALESCE("password", '') = $4"#;
        ctx.execute(sql, &[&channel_id, &char_id, &new, &old]).await;
    }
    Value::None
}

async fn access_control(ctx: &Ctx<'_>, call: &Call) -> Value {
    let channel_id = call.arg_id(0) as i32;
    let accessor = call.arg_id(1) as i32;
    let mode = call.arg_i64(2).unwrap_or(CHTMODE_NOTSPECIFIED) as i32;
    let until = call.arg_i64(3);
    let reason = call.arg_text(4);
    let char_id = me(ctx);

    let allowed = channel_id == char_id || {
        let sql = format!(r#"SELECT 1 FROM "channels" WHERE "channelID" = $1 AND ({OPERATOR_TEST})"#);
        ctx.query_one(&sql, &[&channel_id, &char_id]).await.is_some()
    };
    if !allowed {
        warn!("AccessControl on channel {channel_id} by a non-operator");
        return Value::None;
    }
    let clear = r#"DELETE FROM "channelMods" WHERE "channelID" = $1 AND "accessor" = $2"#;
    ctx.execute(clear, &[&channel_id, &accessor]).await;
    if i64::from(mode) != CHTMODE_NOTSPECIFIED {
        let sql = r#"INSERT INTO "channelMods" ("channelID","accessor","mode","untilWhen","reason","admin")
                     VALUES ($1, $2, $3, $4, $5,
                             (SELECT "characterName" FROM "character_" WHERE "characterID" = $6))"#;
        ctx.execute(sql, &[&channel_id, &accessor, &mode, &until, &reason, &char_id]).await;
    }
    Value::None
}
