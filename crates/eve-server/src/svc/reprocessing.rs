//! `reprocessingSvc`, bound per station (moniker.py:470): the reprocessing plant
//! (base_reprocessingplant.py) and the reprocess dialog (reprocessingsvc.py).
//!
//! The client only displays the numbers it is given. Yields use the formula of
//! the period: the station's efficiency plus 37.5% scaled by Refining (+2% a
//! level) and Refinery Efficiency (+4% a level), capped at 100%; anything that
//! is not ore gets a further +5% a level of Scrapmetal Processing. The station
//! keeps its take (reprocessingStationsTake), less 0.75% per point of standing
//! with the owning corporation. Ore-specific skills are not modelled.

use std::collections::HashMap;

use eve_marshal::Value;
use tokio_postgres::types::ToSql;
use tracing::{error, info};

use crate::services::{Call, Ctx, Outcome};
use crate::svc::bound;
use crate::items::{self, DbResult, ItemTxn, NewItem};
use crate::svc::inventory::FLAG_HANGAR;
use crate::svc::rows::{float, int, key_val, row};

const SKILL_REFINING: i32 = 3385;
const SKILL_REFINERY_EFFICIENCY: i32 = 3389;
const SKILL_SCRAPMETAL_PROCESSING: i32 = 12196;
const FLAG_SKILL: i32 = 7;
const ATTR_SKILL_LEVEL: i32 = 280;

const CATEGORY_BLUEPRINT: i32 = 9;
const CATEGORY_REACTION: i32 = 24;
const CATEGORY_ASTEROID: i32 = 25;
const GROUP_MINERAL: i32 = 18;
const GROUP_ROOKIESHIP: i32 = 237;
const TYPE_CREDITS: i32 = 29;
const TYPE_BOOKMARK: i32 = 51;

/// The same materials config.BulkData.ramtypematerials gives the client.
const MATERIALS_SQL: &str = r#"SELECT "typeID","requiredTypeID","quantity"
                                 FROM "TL2MaterialsForTypeWithActivity"
                                WHERE "activity" = 6 AND "damagePerJob" = 1
                                  AND "typeID" = ANY($1)"#;

pub async fn reprocessing_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let station = bound::parse(&call.service)
        .and_then(|b| b.params.first().copied())
        .or(ctx.session.station_id)?;
    let value = match call.method.as_str() {
        "GetReprocessingInfo" => reprocessing_info(&terms(ctx, station).await?),
        // GetOptionsForItemTypes({typeID: 0}) -> {typeID: option row}.
        "GetOptionsForItemTypes" => {
            let types: Vec<i32> = match call.args.first() {
                Some(Value::Dict(d)) => d.iter().filter_map(|(k, _)| k.as_i64()).map(|t| t as i32).collect(),
                _ => Vec::new(),
            };
            options(ctx, &types).await
        }
        "GetQuotes" => {
            let terms = terms(ctx, station).await?;
            let quotes = quotes(ctx, &terms, station, &item_ids(call)).await;
            Value::Dict(quotes.into_iter().map(|q| (Value::Int(q.item_id), q.to_value())).collect())
        }
        "Reprocess" => return Some(reprocess(ctx, station, &item_ids(call)).await),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

fn item_ids(call: &Call) -> Vec<i64> {
    call.args.first().and_then(|v| v.seq()).unwrap_or(&[]).iter().filter_map(|v| v.as_i64()).collect()
}

/// What this station offers this character.
struct Terms {
    station_yield: f64,
    /// Station and refining skills together.
    combined: f64,
    scrapmetal: f64,
    /// The fraction of the recovered materials the station keeps.
    tax: f64,
    standing: f64,
    char_id: i64,
}

impl Terms {
    fn yield_for(&self, category: i32) -> f64 {
        if category == CATEGORY_ASTEROID {
            self.combined
        } else {
            (self.combined * (1.0 + 0.05 * self.scrapmetal)).min(1.0)
        }
    }
}

async fn terms(ctx: &Ctx<'_>, station: i64) -> Option<Terms> {
    let char_id = ctx.session.char_id?;
    let sql = r#"
        WITH skill AS (
            SELECT e."typeID", coalesce(a."valueInt", a."valueFloat"::bigint) AS level
              FROM entity e
              JOIN entity_attributes a ON a."itemID" = e."itemID" AND a."attributeID" = $3
             WHERE e."ownerID" = $2 AND e."flag" = $4
        )
        SELECT st."reprocessingEfficiency", st."reprocessingStationsTake",
               coalesce((SELECT n."standing" FROM "chrNPCStandings" n
                          WHERE n."characterID" = $2 AND n."fromID" = st."corporationID"), 0),
               coalesce((SELECT max(level) FROM skill WHERE "typeID" = $5), 0)::float8,
               coalesce((SELECT max(level) FROM skill WHERE "typeID" = $6), 0)::float8,
               coalesce((SELECT max(level) FROM skill WHERE "typeID" = $7), 0)::float8
          FROM "staStations" st WHERE st."stationID" = $1"#;
    let params: [&(dyn ToSql + Sync); 7] = [
        &(station as i32),
        &(char_id as i32),
        &ATTR_SKILL_LEVEL,
        &FLAG_SKILL,
        &SKILL_REFINING,
        &SKILL_REFINERY_EFFICIENCY,
        &SKILL_SCRAPMETAL_PROCESSING,
    ];
    let r = ctx.query_one(sql, &params).await?;
    let f = |i: usize| float(&r, i).unwrap_or(0.0);
    let (station_yield, take, standing) = (f(0), f(1), f(2));
    let (refining, efficiency, scrapmetal) = (f(3), f(4), f(5));
    let combined =
        (station_yield + 0.375 * (1.0 + 0.02 * refining) * (1.0 + 0.04 * efficiency)).min(1.0);
    Some(Terms {
        station_yield,
        combined,
        scrapmetal,
        tax: (take - 0.0075 * standing).max(0.0),
        standing,
        char_id,
    })
}

/// {'yield', 'combinedyield', 'wetake': (fraction, standing)}
/// (base_reprocessingplant.py:93-105).
fn reprocessing_info(t: &Terms) -> Value {
    let key = |k: &str| Value::bytes(k);
    Value::Dict(vec![
        (key("yield"), Value::Double(t.station_yield)),
        (key("combinedyield"), Value::Double(t.combined)),
        (key("wetake"), Value::Tuple(vec![Value::Double(t.tax), Value::Double(t.standing)])),
    ])
}

/// The client's own rule (cfg.py GetReprocessingOptions): a type with materials
/// is refinable if it is ore and recyclable otherwise; money, bookmarks,
/// minerals, rookie ships, blueprints and reactions are neither.
async fn options(ctx: &Ctx<'_>, types: &[i32]) -> Value {
    const HEADER: &[&str] = &["typeID", "isRecyclable", "isRefinable"];
    let sql = r#"
        SELECT t."typeID", g."categoryID",
               t."typeID" <> ALL($2) AND t."groupID" <> ALL($3) AND g."categoryID" <> ALL($4)
               AND EXISTS (SELECT 1 FROM "TL2MaterialsForTypeWithActivity" m
                            WHERE m."typeID" = t."typeID" AND m."activity" = 6
                              AND m."damagePerJob" = 1)
          FROM "invTypes" t JOIN "invGroups" g ON g."groupID" = t."groupID"
         WHERE t."typeID" = ANY($1)"#;
    let excluded_types = vec![TYPE_CREDITS, TYPE_BOOKMARK];
    let excluded_groups = vec![GROUP_MINERAL, GROUP_ROOKIESHIP];
    let excluded_categories = vec![CATEGORY_BLUEPRINT, CATEGORY_REACTION];
    let rows = ctx.query(sql, &[&types, &excluded_types, &excluded_groups, &excluded_categories]).await;
    let out = rows
        .iter()
        .map(|r| {
            let type_id = int(r, 0).unwrap_or(0);
            let asteroid = int(r, 1) == Some(CATEGORY_ASTEROID as i64);
            let usable = r.get::<_, Option<bool>>(2).unwrap_or(false);
            let flags = (usable && !asteroid, usable && asteroid);
            let line = vec![Value::Int(type_id), Value::Int(flags.0.into()), Value::Int(flags.1.into())];
            (Value::Int(type_id), row(HEADER, line))
        })
        .collect();
    Value::Dict(out)
}

struct Recoverable {
    type_id: i64,
    client: i64,
    station: i64,
    unrecoverable: i64,
}

struct Quote {
    item_id: i64,
    quantity: i64,
    leftover: i64,
    recoverables: Vec<Recoverable>,
}

impl Quote {
    fn to_value(&self) -> Value {
        let recoverables = self
            .recoverables
            .iter()
            .map(|m| {
                key_val(vec![
                    ("typeID", Value::Int(m.type_id)),
                    ("client", Value::Int(m.client)),
                    ("station", Value::Int(m.station)),
                    ("unrecoverable", Value::Int(m.unrecoverable)),
                ])
            })
            .collect();
        key_val(vec![
            ("itemID", Value::Int(self.item_id)),
            ("quantityToProcess", Value::Int(self.quantity - self.leftover)),
            ("leftOvers", Value::Int(self.leftover)),
            ("recoverables", Value::List(recoverables)),
        ])
    }
}

/// Quotes for those of `item_ids` the character can reprocess here: its own
/// items in the station hangar or in a ship of its docked here, holding at
/// least one portion. The active ship is never offered.
async fn quotes(ctx: &Ctx<'_>, t: &Terms, station: i64, item_ids: &[i64]) -> Vec<Quote> {
    let ids: Vec<i32> = item_ids.iter().map(|i| *i as i32).collect();
    let sql = r#"
        SELECT e."itemID", e."typeID", e."quantity", t."portionSize", g."categoryID"
          FROM entity e
          JOIN "invTypes" t ON t."typeID" = e."typeID"
          JOIN "invGroups" g ON g."groupID" = t."groupID"
         WHERE e."itemID" = ANY($1) AND e."ownerID" = $2
           AND e."itemID" IS DISTINCT FROM $4
           AND (e."locationID" = $3
                OR e."locationID" IN (SELECT s."itemID" FROM entity s
                                       WHERE s."ownerID" = $2 AND s."locationID" = $3))"#;
    let active_ship = ctx.session.ship_id.map(|s| s as i32);
    let items = ctx.query(sql, &[&ids, &(t.char_id as i32), &(station as i32), &active_ship]).await;
    let types: Vec<i32> = items.iter().filter_map(|r| int(r, 1)).map(|v| v as i32).collect();
    let mut materials: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    for r in ctx.query(MATERIALS_SQL, &[&types]).await {
        let (Some(product), Some(material), Some(qty)) = (int(&r, 0), int(&r, 1), int(&r, 2)) else { continue };
        materials.entry(product).or_default().push((material, qty));
    }

    let mut out = Vec::new();
    for r in &items {
        let (Some(item_id), Some(type_id), Some(quantity)) = (int(r, 0), int(r, 1), int(r, 2)) else { continue };
        let portion = int(r, 3).unwrap_or(1).max(1);
        let portions = quantity / portion;
        let Some(mats) = materials.get(&type_id) else { continue };
        if portions == 0 {
            continue;
        }
        let yield_ = t.yield_for(int(r, 4).unwrap_or(0) as i32);
        let recoverables = mats
            .iter()
            .map(|&(material, per_portion)| {
                let total = per_portion * portions;
                let recovered = (total as f64 * yield_).floor() as i64;
                let station_share = (recovered as f64 * t.tax).floor() as i64;
                Recoverable {
                    type_id: material,
                    client: recovered - station_share,
                    station: station_share,
                    unrecoverable: total - recovered,
                }
            })
            .collect();
        out.push(Quote { item_id, quantity, leftover: quantity % portion, recoverables });
    }
    out
}

/// Reprocess(itemIDs): every quotable item is used up (a leftover short of a
/// portion stays), whatever it held goes to the hangar, and the character's
/// share of the materials is stacked in the hangar.
async fn reprocess(ctx: &Ctx<'_>, station: i64, item_ids: &[i64]) -> Outcome {
    let Some(terms) = terms(ctx, station).await else { return Outcome::custom("You are not docked here.") };
    let quotes = quotes(ctx, &terms, station, item_ids).await;
    if quotes.is_empty() {
        return Outcome::custom("None of those items can be reprocessed here.");
    }
    match apply(ctx.db_url, terms.char_id, station, &quotes).await {
        Ok(events) => {
            items::notify(ctx.world, ctx.client_id, events);
            info!("reprocessed {} item(s) at station {station}", quotes.len());
            Outcome::Value(Value::None)
        }
        Err(e) => {
            error!("reprocessing at {station}: {}", crate::logging::chain(&e));
            Outcome::custom("Reprocessing failed; nothing was changed.")
        }
    }
}

async fn apply(db_url: &str, char_id: i64, station: i64, quotes: &[Quote]) -> DbResult<Vec<Value>> {
    let mut client = crate::db::connect(db_url).await?;
    let mut txn = ItemTxn::begin(&mut client).await?;
    let mut gained: HashMap<i64, i64> = HashMap::new();

    for q in quotes {
        if q.leftover > 0 {
            txn.set_quantity(q.item_id, q.leftover).await?;
        } else {
            let held = txn
                .tx()
                .query(r#"SELECT "itemID" FROM entity WHERE "locationID" = $1"#, &[&(q.item_id as i32)])
                .await?;
            for r in &held {
                txn.move_to(r.get::<_, i32>(0).into(), station, FLAG_HANGAR.into()).await?;
            }
            txn.destroy(q.item_id).await?;
        }
        for m in q.recoverables.iter().filter(|m| m.client > 0) {
            *gained.entry(m.type_id).or_default() += m.client;
        }
    }
    for (type_id, quantity) in gained {
        let stack = NewItem {
            type_id,
            owner_id: char_id,
            location_id: station,
            flag: FLAG_HANGAR.into(),
            quantity,
            singleton: false,
        };
        txn.stack_onto(stack).await?;
    }
    txn.commit().await
}
