use eve_marshal::Value;
use tracing::{error, warn};

use crate::services::{Call, Ctx, Outcome};

const SERVICE: &str = "skillMgr";

const FLAG_SKILL: i32 = 7;
const FLAG_SKILL_IN_TRAINING: i32 = 61;
const FLAG_BOOSTER: i32 = 88;
const FLAG_IMPLANT: i32 = 89;

const CATEGORY_SKILL: i32 = 16;
const CATEGORY_IMPLANT: i32 = 20;
const GROUP_BOOSTER: i32 = 303;

const ATTR_PRIMARY: i32 = 180;
const ATTR_SECONDARY: i32 = 181;
const ATTR_SKILL_TIME_CONSTANT: i32 = 275;
const ATTR_SKILL_POINTS: i32 = 276;
const ATTR_SKILL_LEVEL: i32 = 280;

/// The character's trained level in each of `types` (missing ones are 0).
/// Works on a client or inside a transaction.
pub async fn levels(
    db: &impl tokio_postgres::GenericClient,
    char_id: i64,
    types: &[i32],
) -> Result<std::collections::HashMap<i32, i64>, tokio_postgres::Error> {
    let rows = db
        .query(
            r#"SELECT e."typeID", max(coalesce(a."valueInt", a."valueFloat"::bigint))
                 FROM entity e
                 JOIN entity_attributes a ON a."itemID" = e."itemID" AND a."attributeID" = $3
                WHERE e."ownerID" = $1 AND e."flag" IN ($4, $5) AND e."typeID" = ANY($2)
                GROUP BY e."typeID""#,
            &[&(char_id as i32), &types, &ATTR_SKILL_LEVEL, &FLAG_SKILL, &FLAG_SKILL_IN_TRAINING],
        )
        .await?;
    let mut out: std::collections::HashMap<i32, i64> = types.iter().map(|t| (*t, 0)).collect();
    for r in rows {
        out.insert(r.get(0), r.get::<_, Option<i64>>(1).unwrap_or(0));
    }
    Ok(out)
}

pub async fn skill_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    Some(method_call(&call.method, &call.args, ctx).await)
}

async fn method_call(method: &str, args: &[Value], ctx: &Ctx<'_>) -> Outcome {
    let item_id = args.first().and_then(|v| v.as_i64()).unwrap_or(0);
    match method {
        "GetEndOfTraining" => Outcome::Value(end_of_training(ctx, item_id).await),
        "CharStartTrainingSkill" => Outcome::Value(start_training(ctx, item_id).await),
        "CharStopTrainingSkill" => Outcome::Value(stop_training(ctx, item_id).await),
        "CharAddImplant" => Outcome::Value(add_implant(ctx, item_id).await),
        "RemoveImplantFromCharacter" => Outcome::Value(remove_implant(ctx, item_id).await),
        _ => Outcome::Unknown(format!("{}::{}", SERVICE, method)),
    }
}

async fn end_of_training(ctx: &Ctx<'_>, skill_id: i64) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let Some(it) = item(ctx, skill_id).await else { return Value::None };
    if it.owner_id != char_id || it.flag != FLAG_SKILL_IN_TRAINING {
        return Value::None;
    }
    match training_eta(ctx, char_id, skill_id, it.type_id).await {
        Some(eta) => Value::Long(eta),
        None => Value::None,
    }
}

async fn start_training(ctx: &Ctx<'_>, skill_id: i64) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let Some(it) = item(ctx, skill_id).await else {
        warn!("CharStartTrainingSkill: no item {skill_id}");
        return Value::None;
    };
    if it.owner_id != char_id || it.category_id != CATEGORY_SKILL {
        warn!("CharStartTrainingSkill: {skill_id} is not {char_id}'s skill");
        return Value::None;
    }
    let level = attr(ctx, skill_id, it.type_id, ATTR_SKILL_LEVEL).await.unwrap_or(0.0);
    if level >= 5.0 {
        return Value::None;
    }

    let halt = r#"UPDATE "entity" SET "flag" = $1
                   WHERE "ownerID" = $2 AND "flag" = $3 AND "itemID" <> $4"#;
    if let Err(e) = ctx
        .db
        .execute(
            halt,
            &[&FLAG_SKILL, &(char_id as i32), &FLAG_SKILL_IN_TRAINING, &(skill_id as i32)],
        )
        .await
    {
        error!("CharStartTrainingSkill (halt): {err}", err = crate::logging::chain(&e));
        return Value::None;
    }

    let start = r#"UPDATE "entity" SET "flag" = $1 WHERE "itemID" = $2 AND "ownerID" = $3"#;
    if let Err(e) =
        ctx.db.execute(start, &[&FLAG_SKILL_IN_TRAINING, &(skill_id as i32), &(char_id as i32)]).await
    {
        error!("CharStartTrainingSkill (start): {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

async fn stop_training(ctx: &Ctx<'_>, skill_id: i64) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let sql = r#"UPDATE "entity" SET "flag" = $1
                   WHERE "itemID" = $2 AND "ownerID" = $3 AND "flag" = $4"#;
    if let Err(e) = ctx
        .db
        .execute(sql, &[&FLAG_SKILL, &(skill_id as i32), &(char_id as i32), &FLAG_SKILL_IN_TRAINING])
        .await
    {
        error!("CharStopTrainingSkill: {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

async fn add_implant(ctx: &Ctx<'_>, item_id: i64) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let Some(it) = item(ctx, item_id).await else {
        warn!("CharAddImplant: no item {item_id}");
        return Value::None;
    };
    if it.owner_id != char_id || it.category_id != CATEGORY_IMPLANT {
        warn!("CharAddImplant: {item_id} is not {char_id}'s implant");
        return Value::None;
    }
    let flag = if it.group_id == GROUP_BOOSTER { FLAG_BOOSTER } else { FLAG_IMPLANT };
    let sql = r#"UPDATE "entity" SET "locationID" = $1, "flag" = $2
                   WHERE "itemID" = $3 AND "ownerID" = $4"#;
    if let Err(e) =
        ctx.db.execute(sql, &[&(char_id as i32), &flag, &(item_id as i32), &(char_id as i32)]).await
    {
        error!("CharAddImplant: {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

async fn remove_implant(ctx: &Ctx<'_>, item_id: i64) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let Some(it) = item(ctx, item_id).await else { return Value::None };
    if it.owner_id != char_id || it.flag != FLAG_IMPLANT {
        warn!("RemoveImplantFromCharacter: {item_id} is not a plugged implant");
        return Value::None;
    }
    let attrs = r#"DELETE FROM "entity_attributes" WHERE "itemID" = $1"#;
    if let Err(e) = ctx.db.execute(attrs, &[&(item_id as i32)]).await {
        error!("RemoveImplantFromCharacter (attributes): {err}", err = crate::logging::chain(&e));
        return Value::None;
    }
    let row = r#"DELETE FROM "entity" WHERE "itemID" = $1 AND "ownerID" = $2 AND "flag" = $3"#;
    if let Err(e) = ctx.db.execute(row, &[&(item_id as i32), &(char_id as i32), &FLAG_IMPLANT]).await
    {
        error!("RemoveImplantFromCharacter: {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

async fn training_eta(ctx: &Ctx<'_>, char_id: i64, skill_id: i64, type_id: i64) -> Option<i64> {
    let stc = attr(ctx, skill_id, type_id, ATTR_SKILL_TIME_CONSTANT).await.unwrap_or(0.0);
    let sp = attr(ctx, skill_id, type_id, ATTR_SKILL_POINTS).await.unwrap_or(0.0);
    let level = attr(ctx, skill_id, type_id, ATTR_SKILL_LEVEL).await.unwrap_or(0.0);
    let sp_hi = sp_for_level(stc * 250.0, level)?;

    let primary = attr(ctx, skill_id, type_id, ATTR_PRIMARY).await.unwrap_or(0.0) as i32;
    let secondary = attr(ctx, skill_id, type_id, ATTR_SECONDARY).await.unwrap_or(0.0) as i32;
    let mut spm = 0.0;
    if primary != 0 {
        spm += char_attr(ctx, char_id, primary).await;
    }
    if secondary != 0 {
        spm += char_attr(ctx, char_id, secondary).await / 2.0;
    }
    if spm.is_nan() || spm <= 0.0 {
        return None;
    }

    let minutes = ((sp_hi - sp) / spm).max(0.0);
    if !minutes.is_finite() {
        return None;
    }
    Some(crate::bulkdata::filetime_now() + (minutes * 60.0 * 10_000_000.0) as i64)
}

fn sp_for_level(stc: f64, level: f64) -> Option<f64> {
    if level >= 5.0 {
        None
    } else if level < 0.0 {
        Some(0.0)
    } else if level == 0.0 {
        Some(stc)
    } else {
        Some(stc * 2.0f64.powf(2.5 * level))
    }
}

struct Item {
    type_id: i64,
    owner_id: i64,
    flag: i32,
    group_id: i32,
    category_id: i32,
}

async fn item(ctx: &Ctx<'_>, item_id: i64) -> Option<Item> {
    let sql = r#"SELECT e."typeID", e."ownerID", e."flag", g."groupID", g."categoryID"
                   FROM "entity" e
                   JOIN "invTypes" t ON t."typeID" = e."typeID"
                   JOIN "invGroups" g ON g."groupID" = t."groupID"
                  WHERE e."itemID" = $1"#;
    match ctx.db.query_opt(sql, &[&(item_id as i32)]).await {
        Ok(Some(r)) => Some(Item {
            type_id: r.get::<_, i32>(0) as i64,
            owner_id: r.get::<_, i32>(1) as i64,
            flag: r.get::<_, i32>(2),
            group_id: r.get::<_, i32>(3),
            category_id: r.get::<_, i32>(4),
        }),
        Ok(None) => None,
        Err(e) => {
            error!("item {item_id}: {err}", err = crate::logging::chain(&e));
            None
        }
    }
}

async fn attr(ctx: &Ctx<'_>, item_id: i64, type_id: i64, attribute_id: i32) -> Option<f64> {
    let sql = r#"SELECT COALESCE(
                     (SELECT COALESCE(ea."valueFloat", ea."valueInt"::double precision)
                        FROM "entity_attributes" ea
                       WHERE ea."itemID" = $1 AND ea."attributeID" = $3),
                     (SELECT COALESCE(ta."valueFloat", ta."valueInt"::double precision)
                        FROM "dgmTypeAttributes" ta
                       WHERE ta."typeID" = $2 AND ta."attributeID" = $3),
                     (SELECT at."defaultValue"::double precision
                        FROM "dgmAttributeTypes" at
                       WHERE at."attributeID" = $3))"#;
    match ctx.db.query_opt(sql, &[&(item_id as i32), &(type_id as i32), &attribute_id]).await {
        Ok(Some(r)) => r.get::<_, Option<f64>>(0),
        Ok(None) => None,
        Err(e) => {
            error!("attribute {attribute_id} of {item_id}: {err}", err = crate::logging::chain(&e));
            None
        }
    }
}

async fn char_attr(ctx: &Ctx<'_>, char_id: i64, attribute_id: i32) -> f64 {
    let own = r#"SELECT COALESCE(ea."valueFloat", ea."valueInt"::double precision)
                   FROM "entity_attributes" ea
                  WHERE ea."itemID" = $1 AND ea."attributeID" = $2"#;
    if let Ok(Some(r)) = ctx.db.query_opt(own, &[&(char_id as i32), &attribute_id]).await {
        if let Some(v) = r.get::<_, Option<f64>>(0) {
            return v;
        }
    }

    let column = match attribute_id {
        164 => "charisma",
        165 => "intelligence",
        166 => "memory",
        167 => "perception",
        168 => "willpower",
        _ => return 0.0,
    };
    let sql = r#"SELECT b."charisma", b."intelligence", b."memory", b."perception", b."willpower"
                   FROM "character_" c
                   JOIN "chrBloodlines" b ON b."bloodlineID" = c."bloodlineID"
                  WHERE c."characterID" = $1"#;
    match ctx.db.query_opt(sql, &[&(char_id as i32)]).await {
        Ok(Some(r)) => {
            let idx = match column {
                "charisma" => 0,
                "intelligence" => 1,
                "memory" => 2,
                "perception" => 3,
                _ => 4,
            };
            r.get::<_, Option<i32>>(idx).map(|v| v as f64).unwrap_or(0.0)
        }
        Ok(None) => 0.0,
        Err(e) => {
            error!("character attribute {attribute_id} of {char_id}: {err}", err = crate::logging::chain(&e));
            0.0
        }
    }
}
