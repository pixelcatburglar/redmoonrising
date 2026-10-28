use eve_marshal::Value;
use tokio_postgres::Row as PgRow;
use tracing::{error, warn};

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{row, rowset};

pub async fn charmgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetPublicInfo" => public_info(ctx, subject(call, ctx)).await,
        "GetPublicInfo3" => public_info3(ctx, subject(call, ctx)).await,
        "GetPrivateInfo" => private_info(ctx, subject(call, ctx)).await,
        "GetCharacterInfo" => character_info(ctx).await,
        "GetTopBounties" => top_bounties(ctx).await,
        "AddToBounty" => return Some(add_to_bounty(ctx, call).await),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

/// AddToBounty(charID, amount): the caller pays; the bounty goes on the target.
async fn add_to_bounty(ctx: &Ctx<'_>, call: &Call) -> Outcome {
    use crate::svc::wallet::{move_cash, Payment, Transfer, REF_BOUNTY};
    let target = call.arg_id(0);
    let amount = call.args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let Some(me) = ctx.session.char_id else { return Outcome::Value(Value::None) };
    if !amount.is_finite() || amount <= 0.0 || target == me {
        return Outcome::Value(Value::None);
    }
    let payment =
        Payment { payer: me, payee: None, amount, ref_type: REF_BOUNTY, arg_id1: "", reason: "" };
    match move_cash(ctx.db_url, payment).await {
        Ok(()) => {}
        Err(Transfer::NotEnough { need, have }) => return Outcome::not_enough_money(need, have),
        Err(Transfer::NoSuchOwner) => return Outcome::Value(Value::None),
        Err(Transfer::Db(e)) => {
            error!("AddToBounty payment: {}", crate::logging::chain(&e));
            return Outcome::Value(Value::None);
        }
    }
    let now = crate::bulkdata::filetime_now();
    let sql = r#"WITH placed AS (
                     INSERT INTO "bounties" ("characterID","ownerID","bounty","placedAt")
                     VALUES ($1,$2,$3,$4)
                     ON CONFLICT ("characterID","ownerID")
                     DO UPDATE SET "bounty" = "bounties"."bounty" + EXCLUDED."bounty",
                                   "placedAt" = EXCLUDED."placedAt")
                 UPDATE "character_" SET "bounty" = "bounty" + $3 WHERE "characterID" = $1"#;
    ctx.execute(sql, &[&(target as i32), &(me as i32), &amount, &now]).await;
    tracing::info!("bounty of {amount} ISK placed on {target}");
    Outcome::Value(Value::None)
}

pub async fn character(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetPrivateInfo" => private_info(ctx, subject(call, ctx)).await,
        "GetCharacterInfo" => character_info(ctx).await,
        "GetCharacterDescription" => character_description(ctx, subject(call, ctx)).await,
        "SetCharacterDescription" => set_character_description(call, ctx).await,
        "GetCloneTypeID" => clone_type_id(ctx).await,
        "GetHomeStation" => home_station(ctx),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

fn subject(call: &Call, ctx: &Ctx<'_>) -> i64 {
    call.args
        .first()
        .and_then(|v| v.as_i64())
        .or(ctx.session.char_id)
        .unwrap_or(0)
}



fn empty_row(header: &[&str]) -> Value {
    row(header, header.iter().map(|_| Value::None).collect())
}

fn int_col(r: &PgRow, i: usize) -> Value {
    match r.try_get::<_, Option<i32>>(i) {
        Ok(Some(v)) => Value::Int(v as i64),
        _ => Value::None,
    }
}

fn small_col(r: &PgRow, i: usize) -> Value {
    match r.try_get::<_, Option<i16>>(i) {
        Ok(Some(v)) => Value::Int(v as i64),
        _ => Value::None,
    }
}

fn long_col(r: &PgRow, i: usize) -> Value {
    match r.try_get::<_, Option<i64>>(i) {
        Ok(Some(v)) => Value::Long(v),
        _ => Value::None,
    }
}

fn dbl_col(r: &PgRow, i: usize) -> Value {
    match r.try_get::<_, Option<f64>>(i) {
        Ok(Some(v)) => Value::Double(v),
        _ => Value::None,
    }
}

fn txt_col(r: &PgRow, i: usize) -> Value {
    match r.try_get::<_, Option<String>>(i) {
        Ok(Some(v)) => Value::Bytes(v.into_bytes()),
        _ => Value::None,
    }
}

async fn public_info(ctx: &Ctx<'_>, char_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "characterID",
        "charName",
        "raceID",
        "raceName",
        "bloodlineID",
        "bloodlineName",
        "gender",
        "corporationID",
        "createDateTime",
    ];
    let sql = r#"SELECT c."characterID", c."characterName",
                        COALESCE(NULLIF(c."raceID", 0), b."raceID") AS "raceID",
                        r."raceName", c."bloodlineID", b."bloodlineName",
                        c."gender", c."corporationID", c."createDateTime"
                   FROM "character_" c
                   LEFT JOIN "chrBloodlines" b ON b."bloodlineID" = c."bloodlineID"
                   LEFT JOIN "chrRaces" r
                          ON r."raceID" = COALESCE(NULLIF(c."raceID", 0), b."raceID")
                  WHERE c."characterID" = $1"#;
    match ctx.db.query(sql, &[&(char_id as i32)]).await {
        Ok(rows) => match rows.first() {
            Some(r) => row(
                HEADER,
                vec![
                    int_col(r, 0),
                    txt_col(r, 1),
                    int_col(r, 2),
                    txt_col(r, 3),
                    int_col(r, 4),
                    txt_col(r, 5),
                    small_col(r, 6),
                    int_col(r, 7),
                    long_col(r, 8),
                ],
            ),
            None => empty_row(HEADER),
        },
        Err(e) => {
            error!("charmgr.GetPublicInfo({char_id}): {err}", err = crate::logging::chain(&e));
            empty_row(HEADER)
        }
    }
}

async fn public_info3(ctx: &Ctx<'_>, char_id: i64) -> Value {
    const HEADER: &[&str] = &["description", "startDateTime", "bounty"];
    let sql = r#"SELECT "description",
                        CASE WHEN "corporationDateTime" > 0
                             THEN "corporationDateTime" ELSE "startDateTime" END,
                        "bounty"
                   FROM "character_" WHERE "characterID" = $1"#;
    match ctx.db.query(sql, &[&(char_id as i32)]).await {
        Ok(rows) => {
            let lines = rows
                .iter()
                .map(|r| Value::List(vec![txt_col(r, 0), long_col(r, 1), dbl_col(r, 2)]))
                .collect();
            rowset(HEADER, lines)
        }
        Err(e) => {
            error!("charmgr.GetPublicInfo3({char_id}): {err}", err = crate::logging::chain(&e));
            rowset(HEADER, Vec::new())
        }
    }
}

async fn private_info(ctx: &Ctx<'_>, char_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "characterID",
        "charName",
        "corporationID",
        "title",
        "description",
        "balance",
        "bounty",
        "securityRating",
        "logonMinutes",
        "createDateTime",
        "startDateTime",
        "intelligence",
        "memory",
        "charisma",
        "perception",
        "willpower",
    ];
    let sql = r#"SELECT c."characterID", c."characterName", c."corporationID", c."title",
                        c."description", c."balance", c."bounty", c."securityRating",
                        c."logonMinutes", c."createDateTime", c."startDateTime",
                        b."intelligence", b."memory", b."charisma", b."perception",
                        b."willpower"
                   FROM "character_" c
                   LEFT JOIN "chrBloodlines" b ON b."bloodlineID" = c."bloodlineID"
                  WHERE c."characterID" = $1"#;
    match ctx.db.query(sql, &[&(char_id as i32)]).await {
        Ok(rows) => match rows.first() {
            Some(r) => row(
                HEADER,
                vec![
                    int_col(r, 0),
                    txt_col(r, 1),
                    int_col(r, 2),
                    txt_col(r, 3),
                    txt_col(r, 4),
                    dbl_col(r, 5),
                    dbl_col(r, 6),
                    dbl_col(r, 7),
                    int_col(r, 8),
                    long_col(r, 9),
                    long_col(r, 10),
                    int_col(r, 11),
                    int_col(r, 12),
                    int_col(r, 13),
                    int_col(r, 14),
                    int_col(r, 15),
                ],
            ),
            None => empty_row(HEADER),
        },
        Err(e) => {
            error!("charmgr.GetPrivateInfo({char_id}): {err}", err = crate::logging::chain(&e));
            empty_row(HEADER)
        }
    }
}

async fn character_info(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["characterID", "characterName"];
    let Some(user_id) = ctx.session.user_id else { return rowset(HEADER, Vec::new()) };
    let sql = r#"SELECT "characterID","characterName" FROM "character_"
                  WHERE "accountID" = $1 ORDER BY "characterID""#;
    match ctx.db.query(sql, &[&(user_id as i32)]).await {
        Ok(rows) => {
            let lines = rows
                .iter()
                .map(|r| Value::List(vec![int_col(r, 0), txt_col(r, 1)]))
                .collect();
            rowset(HEADER, lines)
        }
        Err(e) => {
            error!("character.GetCharacterInfo: {err}", err = crate::logging::chain(&e));
            rowset(HEADER, Vec::new())
        }
    }
}

async fn top_bounties(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["characterID", "ownerName", "bounty", "online"];
    let sql = r#"SELECT "characterID","characterName","bounty" FROM "character_"
                  WHERE "bounty" > 0 ORDER BY "bounty" DESC LIMIT 10"#;
    match ctx.db.query(sql, &[]).await {
        Ok(rows) => {
            let lines = rows
                .iter()
                .map(|r| {
                    Value::List(vec![int_col(r, 0), txt_col(r, 1), dbl_col(r, 2), Value::Int(0)])
                })
                .collect();
            rowset(HEADER, lines)
        }
        Err(e) => {
            error!("charmgr.GetTopBounties: {err}", err = crate::logging::chain(&e));
            rowset(HEADER, Vec::new())
        }
    }
}

async fn character_description(ctx: &Ctx<'_>, char_id: i64) -> Value {
    const HEADER: &[&str] = &["description"];
    let sql = r#"SELECT "description" FROM "character_" WHERE "characterID" = $1"#;
    match ctx.db.query(sql, &[&(char_id as i32)]).await {
        Ok(rows) => match rows.first() {
            Some(r) => row(HEADER, vec![txt_col(r, 0)]),
            None => Value::None,
        },
        Err(e) => {
            error!("character.GetCharacterDescription({char_id}): {err}", err = crate::logging::chain(&e));
            Value::None
        }
    }
}

async fn set_character_description(call: &Call, ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let text = match call.args.first() {
        Some(Value::Bytes(b)) | Some(Value::Buffer(b)) => String::from_utf8_lossy(b).into_owned(),
        Some(Value::Unicode(s)) => s.clone(),
        _ => String::new(),
    };
    let text: String = text.chars().take(4000).collect();
    let sql = r#"UPDATE "character_" SET "description" = $1 WHERE "characterID" = $2"#;
    if let Err(e) = ctx.db.execute(sql, &[&text, &(char_id as i32)]).await {
        error!("character.SetCharacterDescription({char_id}): {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

fn home_station(ctx: &Ctx<'_>) -> Value {
    match ctx.session.hq_id.or(ctx.session.station_id) {
        Some(id) => Value::Int(id),
        None => {
            warn!("character.GetHomeStation: no hqID or stationID in session");
            Value::None
        }
    }
}

async fn clone_type_id(ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let sql = r#"SELECT e."typeID" FROM "entity" e
                   JOIN "invTypes" t ON t."typeID" = e."typeID"
                  WHERE e."ownerID" = $1 AND t."groupID" = 23
                  ORDER BY e."typeID" DESC LIMIT 1"#;
    match ctx.db.query(sql, &[&(char_id as i32)]).await {
        Ok(rows) => match rows.first() {
            Some(r) => int_col(r, 0),
            None => Value::None,
        },
        Err(e) => {
            error!("character.GetCloneTypeID({char_id}): {err}", err = crate::logging::chain(&e));
            Value::None
        }
    }
}
