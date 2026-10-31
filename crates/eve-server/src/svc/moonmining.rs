//! Starbase production (moonmining.py's Production tab): moon harvesting
//! arrays mine the moon a tower stands at, silos hold what they are given,
//! and reactors turn what their silos hold into something else.
//!
//! Each harvester mines one of its moon's materials and each silo holds one
//! type, both chosen with ChangeStructureProvisionType; LinkResourceForTower
//! links what a structure puts out to the structure that takes it in
//! (`posLinks`). A reactor runs the reaction whose item sits in it. Every
//! time the tower pays for another period of fuel, its online structures
//! run one cycle: harvesters first, then reactors.
//!
//! The database has no record of what each moon holds, so it is drawn from
//! the moon's ID: up to three materials, the rarer ones less often, the
//! rarest only where security is at or below zero.

use std::collections::HashMap;
use std::sync::OnceLock;

use eve_marshal::Value;
use tokio_postgres::{Client, GenericClient};
use tracing::{error, info};

use crate::items::{ItemTxn, NewItem};
use crate::pos::state;
use crate::services::{Ctx, Outcome};
use crate::svc::membership::Refused;
use crate::svc::rows::{float, int};

pub const GROUP_MOON_MINING: i64 = 416;
pub const GROUP_SILO: i64 = 404;
pub const GROUP_REACTOR: i64 = 438;
const GROUP_MOON_MATERIALS: i32 = 427;
const ATTR_CARGO_GROUP: i32 = 629;
const ATTR_HARVESTER_QUALITY: i32 = 710;
const ATTR_MOON_MINING_AMOUNT: i32 = 726;
const ATTR_REACTION_GROUP: [i32; 2] = [842, 843];
/// Moon material tiers by base price, with how often each turns up.
const TIERS: [(f64, u32); 5] = [(2.0, 45), (8.0, 25), (16.0, 15), (64.0, 10), (256.0, 5)];
/// How many materials a moon holds, with how often: none, one, two, three.
const COUNTS: [u32; 4] = [30, 40, 20, 10];

static MATERIALS: OnceLock<Vec<(i64, f64)>> = OnceLock::new();

/// The moon materials and their base prices, read once.
async fn materials(db: &impl GenericClient) -> &'static [(i64, f64)] {
    if let Some(m) = MATERIALS.get() {
        return m;
    }
    let sql = r#"SELECT "typeID", "basePrice" FROM "invTypes" WHERE "groupID" = $1 ORDER BY "typeID""#;
    let found = match db.query(sql, &[&GROUP_MOON_MATERIALS]).await {
        Ok(rows) => rows.iter().filter_map(|r| Some((int(r, 0)?, float(r, 1)?))).collect(),
        Err(e) => {
            error!("reading the moon materials: {}", crate::logging::chain(&e));
            return &[];
        }
    };
    MATERIALS.get_or_init(|| found)
}

/// What a moon holds: (material, batches per cycle).
pub fn moon_materials(moon_id: i64, security: f64, materials: &[(i64, f64)]) -> Vec<(i64, i64)> {
    let mut rng = crate::combat::Rng::seeded(moon_id as u64 ^ 0x6d6f_6f6e);
    let pick = |rng: &mut crate::combat::Rng, weights: &[u32]| {
        let total: u32 = weights.iter().sum();
        let mut roll = rng.below(total as usize) as u32;
        weights.iter().position(|w| {
            let hit = roll < *w;
            roll = roll.saturating_sub(*w);
            hit
        })
    };
    let count = pick(&mut rng, &COUNTS).unwrap_or(0);
    let mut out: Vec<(i64, i64)> = Vec::new();
    for _ in 0..count {
        let Some(mut tier) = pick(&mut rng, &TIERS.map(|(_, w)| w)) else { continue };
        if tier == TIERS.len() - 1 && security > 0.0 {
            tier -= 1;
        }
        let in_tier: Vec<i64> = materials.iter().filter(|(_, p)| (*p - TIERS[tier].0).abs() < 1e-6).map(|(t, _)| *t).collect();
        if in_tier.is_empty() {
            continue;
        }
        let t = in_tier[rng.below(in_tier.len())];
        if !out.iter().any(|(o, _)| *o == t) {
            out.push((t, 1));
        }
    }
    out
}

/// The moon a tower stands at and its system's security.
async fn tower_moon(db: &impl GenericClient, tower: i64) -> Option<(i64, f64)> {
    let sql = r#"SELECT p."moonID", COALESCE(s."security", 0)
                   FROM "posStructures" p
                   JOIN entity e ON e."itemID" = p."itemID"
                   LEFT JOIN "mapSolarSystems" s ON s."solarSystemID" = e."locationID"
                  WHERE p."itemID" = $1"#;
    let r = db.query_opt(sql, &[&(tower as i32)]).await.ok().flatten()?;
    Some((int(&r, 0)?, float(&r, 1).unwrap_or(0.0)))
}

/// GetMoonForTower(towerID): (moonID, [(typeID, batches)]); the materials
/// only once the tower is online and can scan the moon.
pub async fn moon_for_tower(ctx: &Ctx<'_>, tower: i64) -> Value {
    let Some((moon, security)) = tower_moon(ctx.db, tower).await else { return Value::None };
    let online = ctx
        .query_one(r#"SELECT 1 FROM "posStructures" WHERE "itemID" = $1 AND "state" IN (3, 4, 5)"#, &[&(tower as i32)])
        .await
        .is_some();
    if !online {
        return Value::Tuple(vec![Value::Int(moon), Value::None]);
    }
    let held = moon_materials(moon, security, materials(ctx.db).await);
    let list = held.iter().map(|(t, q)| Value::Tuple(vec![Value::Int(*t), Value::Int(*q)])).collect();
    Value::Tuple(vec![Value::Int(moon), Value::List(list)])
}

/// A production structure of a tower, as the database has it.
#[derive(Debug, Clone)]
struct Producer {
    id: i64,
    group_id: i64,
    type_id: i64,
    owner_id: i64,
    online: bool,
    provision: Option<i64>,
}

async fn producers(db: &impl GenericClient, tower: i64) -> Result<Vec<Producer>, tokio_postgres::Error> {
    let sql = r#"SELECT p."itemID", t."groupID", e."typeID", e."ownerID", p."state", p."provisionType"
                   FROM "posStructures" p
                   JOIN entity e ON e."itemID" = p."itemID"
                   JOIN "invTypes" t ON t."typeID" = e."typeID"
                  WHERE p."controlTowerID" = $1 AND t."groupID" = ANY($2)
                  ORDER BY p."itemID""#;
    let groups = vec![GROUP_MOON_MINING as i32, GROUP_SILO as i32, GROUP_REACTOR as i32];
    Ok(db
        .query(sql, &[&(tower as i32), &groups])
        .await?
        .iter()
        .map(|r| Producer {
            id: int(r, 0).unwrap_or(0),
            group_id: int(r, 1).unwrap_or(0),
            type_id: int(r, 2).unwrap_or(0),
            owner_id: int(r, 3).unwrap_or(0),
            online: matches!(int(r, 4).map(|s| s as i16), Some(state::ONLINING | state::REINFORCED | state::ONLINE | state::OPERATING)),
            provision: int(r, 5),
        })
        .collect())
}

/// The reaction item sitting in a reactor, of a group the reactor runs
/// (reactionGroup1/2): (itemID, reaction typeID).
async fn reaction_in(db: &impl GenericClient, reactor: i64) -> Option<(i64, i64)> {
    let sql = r#"SELECT e."itemID", e."typeID" FROM entity e
                   JOIN "invTypes" t ON t."typeID" = e."typeID"
                  WHERE e."locationID" = $1
                    AND EXISTS (SELECT 1 FROM "invTypeReactions" r WHERE r."reactionTypeID" = e."typeID")
                    AND t."groupID" IN (SELECT COALESCE(a."valueFloat", a."valueInt"::float8)::int
                                          FROM "dgmTypeAttributes" a JOIN entity r ON r."typeID" = a."typeID"
                                         WHERE r."itemID" = $1 AND a."attributeID" = ANY($2))
                  ORDER BY e."itemID" LIMIT 1"#;
    let r = db.query_opt(sql, &[&(reactor as i32), &ATTR_REACTION_GROUP.to_vec()]).await.ok().flatten()?;
    Some((int(&r, 0)?, int(&r, 1)?))
}

/// A reaction's (inputs, outputs): (typeID, batches).
async fn reaction_rows(db: &impl GenericClient, reaction: i64) -> (Vec<(i64, i64)>, Vec<(i64, i64)>) {
    let sql = r#"SELECT "input", "typeID", "quantity" FROM "invTypeReactions" WHERE "reactionTypeID" = $1"#;
    let rows = db.query(sql, &[&(reaction as i32)]).await.unwrap_or_default();
    let (mut inputs, mut outputs) = (Vec::new(), Vec::new());
    for r in rows {
        let row = (int(&r, 1).unwrap_or(0), int(&r, 2).unwrap_or(0));
        if int(&r, 0) == Some(1) { inputs.push(row) } else { outputs.push(row) }
    }
    (inputs, outputs)
}

async fn links(db: &impl GenericClient, sources: &[i64]) -> Vec<(i64, i64, i64)> {
    let ids: Vec<i32> = sources.iter().map(|i| *i as i32).collect();
    let sql = r#"SELECT "sourceID", "typeID", "destinationID" FROM "posLinks" WHERE "sourceID" = ANY($1)"#;
    db.query(sql, &[&ids])
        .await
        .map(|rows| rows.iter().filter_map(|r| Some((int(r, 0)?, int(r, 1)?, int(r, 2)?))).collect())
        .unwrap_or_default()
}

async fn type_attribute(db: &impl GenericClient, types: &[i64], attr: i32) -> HashMap<i64, f64> {
    let types: Vec<i32> = types.iter().map(|t| *t as i32).collect();
    let sql = r#"SELECT "typeID", COALESCE("valueFloat", "valueInt"::float8) FROM "dgmTypeAttributes"
                  WHERE "typeID" = ANY($1) AND "attributeID" = $2"#;
    db.query(sql, &[&types, &attr])
        .await
        .map(|rows| rows.iter().filter_map(|r| Some((int(r, 0)?, float(r, 1)?))).collect())
        .unwrap_or_default()
}

/// GetMoonProcessInfoForTower(towerID): for each harvester, silo and reactor,
/// (itemID, active, reaction, incoming [(sourceID, typeID)], demands,
/// supplies), demands and supplies as [(typeID, quantity)], -1 for "any".
/// A reactor's are left for the client to read from its reaction.
pub async fn process_info(ctx: &Ctx<'_>, tower: i64) -> Value {
    let Ok(all) = producers(ctx.db, tower).await else { return Value::List(Vec::new()) };
    let ids: Vec<i64> = all.iter().map(|p| p.id).collect();
    let links = links(ctx.db, &ids).await;
    let quality = type_attribute(ctx.db, &all.iter().map(|p| p.type_id).collect::<Vec<_>>(), ATTR_HARVESTER_QUALITY).await;
    let mut out = Vec::new();
    for p in &all {
        let incoming: Vec<Value> = links
            .iter()
            .filter(|(_, _, d)| *d == p.id)
            .map(|(s, t, _)| Value::Tuple(vec![Value::Int(*s), Value::Int(*t)]))
            .collect();
        let pair = |t: i64, q: i64| Value::List(vec![Value::Tuple(vec![Value::Int(t), Value::Int(q)])]);
        let none = || Value::List(Vec::new());
        let (reaction, demands, supplies) = match p.group_id {
            GROUP_MOON_MINING => {
                let supplies = p.provision.map_or_else(none, |t| pair(t, quality.get(&p.type_id).copied().unwrap_or(1.0) as i64));
                (Value::None, none(), supplies)
            }
            GROUP_SILO => match p.provision {
                Some(t) => (Value::None, pair(t, -1), pair(t, -1)),
                None => (Value::None, none(), none()),
            },
            _ => {
                let reaction = reaction_in(ctx.db, p.id).await;
                (reaction.map_or(Value::None, |(i, t)| Value::Tuple(vec![Value::Int(i), Value::Int(t)])), Value::None, Value::None)
            }
        };
        out.push(Value::Tuple(vec![Value::Int(p.id), Value::Bool(p.online), reaction, Value::List(incoming), demands, supplies]));
    }
    Value::List(out)
}

/// ChangeStructureProvisionType(towerID, itemID, typeID): what a harvester
/// mines (one of its moon's materials) or a silo holds (of its cargo group,
/// if it has one). Links of the old type go.
pub async fn change_provision(ctx: &Ctx<'_>, tower: i64, item: i64, type_id: i64) -> Refused<Value> {
    let all = producers(ctx.db, tower).await.map_err(|e| crate::svc::membership::db_failed("reading the structures", e))?;
    let Some(p) = all.iter().find(|p| p.id == item) else { return Err(Outcome::custom("That structure is not part of this tower.")) };
    match p.group_id {
        GROUP_MOON_MINING => {
            let (moon, security) = tower_moon(ctx.db, tower).await.ok_or_else(|| Outcome::custom("The tower stands at no moon."))?;
            if !moon_materials(moon, security, materials(ctx.db).await).iter().any(|(t, _)| *t == type_id) {
                return Err(Outcome::custom("This moon does not hold that material."));
            }
        }
        GROUP_SILO => {
            let wanted = type_attribute(ctx.db, &[p.type_id], ATTR_CARGO_GROUP).await.get(&p.type_id).map(|g| *g as i64);
            if let Some(group) = wanted {
                let sql = r#"SELECT 1 FROM "invTypes" WHERE "typeID" = $1 AND "groupID" = $2"#;
                if ctx.query_one(sql, &[&(type_id as i32), &(group as i32)]).await.is_none() {
                    return Err(Outcome::custom("That silo cannot hold that."));
                }
            }
        }
        _ => return Err(Outcome::custom("Only harvesters and silos are set to a type.")),
    }
    let saved = ctx
        .db
        .execute(r#"UPDATE "posStructures" SET "provisionType" = $2 WHERE "itemID" = $1"#, &[&(item as i32), &(type_id as i32)])
        .await
        .and(
            ctx.db
                .execute(
                    r#"DELETE FROM "posLinks" WHERE ("sourceID" = $1 OR "destinationID" = $1) AND "typeID" <> $2"#,
                    &[&(item as i32), &(type_id as i32)],
                )
                .await,
        );
    saved.map_err(|e| crate::svc::membership::db_failed("setting the type", e))?;
    info!("structure {item} at tower {tower} now handles type {type_id}");
    Ok(Value::None)
}

/// LinkResourceForTower(towerID, [((typeID, sourceID), destinationID)]):
/// the tower's links, all at once. Links that make no sense are dropped;
/// the answer is whether all of them were kept.
pub async fn link(ctx: &Ctx<'_>, tower: i64, wanted: Option<&Value>) -> Refused<Value> {
    let all = producers(ctx.db, tower).await.map_err(|e| crate::svc::membership::db_failed("reading the structures", e))?;
    let by_id: HashMap<i64, &Producer> = all.iter().map(|p| (p.id, p)).collect();
    let mut reactions: HashMap<i64, (Vec<(i64, i64)>, Vec<(i64, i64)>)> = HashMap::new();
    for p in all.iter().filter(|p| p.group_id == GROUP_REACTOR) {
        if let Some((_, reaction)) = reaction_in(ctx.db, p.id).await {
            reactions.insert(p.id, reaction_rows(ctx.db, reaction).await);
        }
    }
    let supplies = |p: &Producer, t: i64| match p.group_id {
        GROUP_REACTOR => reactions.get(&p.id).is_some_and(|(_, out)| out.iter().any(|(o, _)| *o == t)),
        _ => p.provision == Some(t),
    };
    let demands = |p: &Producer, t: i64| match p.group_id {
        GROUP_REACTOR => reactions.get(&p.id).is_some_and(|(inp, _)| inp.iter().any(|(i, _)| *i == t)),
        GROUP_SILO => p.provision == Some(t),
        _ => false,
    };
    let mut kept = Vec::new();
    let mut all_kept = true;
    for l in wanted.and_then(|v| v.seq()).unwrap_or(&[]) {
        let parts = l.seq().unwrap_or(&[]);
        let key = parts.first().and_then(|k| k.seq()).unwrap_or(&[]);
        let (t, source, dest) = (
            key.first().and_then(|v| v.as_i64()),
            key.get(1).and_then(|v| v.as_i64()),
            parts.get(1).and_then(|v| v.as_i64()),
        );
        let (Some(t), Some(s), Some(d)) = (t, source, dest) else { all_kept = false; continue };
        match (by_id.get(&s), by_id.get(&d)) {
            (Some(src), Some(dst)) if s != d && supplies(src, t) && demands(dst, t) => kept.push((s, t, d)),
            _ => all_kept = false,
        }
    }
    let ids: Vec<i32> = all.iter().map(|p| p.id as i32).collect();
    let saved = async {
        ctx.db.execute(r#"DELETE FROM "posLinks" WHERE "sourceID" = ANY($1)"#, &[&ids]).await?;
        for (s, t, d) in &kept {
            ctx.db
                .execute(
                    r#"INSERT INTO "posLinks" ("sourceID", "typeID", "destinationID") VALUES ($1, $2, $3)
                       ON CONFLICT ("sourceID", "typeID") DO UPDATE SET "destinationID" = $3"#,
                    &[&(*s as i32), &(*t as i32), &(*d as i32)],
                )
                .await?;
        }
        Ok::<_, tokio_postgres::Error>(())
    }
    .await;
    saved.map_err(|e| crate::svc::membership::db_failed("linking the structures", e))?;
    info!("tower {tower}: {} production link(s)", kept.len());
    Ok(Value::Bool(all_kept))
}

/// One production cycle at a tower, when it has paid for a period of fuel.
pub async fn cycle(db: &mut Client, tower: i64) {
    if let Err(e) = run(db, tower).await {
        error!("production at tower {tower}: {}", crate::logging::chain(&e));
    }
}

async fn run(db: &mut Client, tower: i64) -> Result<(), tokio_postgres::Error> {
    let all: Vec<Producer> = producers(&*db, tower).await?.into_iter().filter(|p| p.online).collect();
    if all.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = all.iter().map(|p| p.id).collect();
    let links = links(&*db, &ids).await;
    let by_id: HashMap<i64, Producer> = all.iter().map(|p| (p.id, p.clone())).collect();
    let moon = match tower_moon(&*db, tower).await {
        Some((moon, security)) => moon_materials(moon, security, materials(&*db).await),
        None => Vec::new(),
    };
    let mut reactions = HashMap::new();
    for p in all.iter().filter(|p| p.group_id == GROUP_REACTOR) {
        if let Some((_, reaction)) = reaction_in(&*db, p.id).await {
            reactions.insert(p.id, reaction_rows(&*db, reaction).await);
        }
    }
    let mut types: Vec<i64> = moon.iter().map(|(t, _)| *t).collect();
    types.extend(reactions.values().flat_map(|(i, o)| i.iter().chain(o).map(|(t, _)| *t)));
    let amount = type_attribute(&*db, &types, ATTR_MOON_MINING_AMOUNT).await;
    let units = |t: i64, batches: i64| batches * amount.get(&t).copied().unwrap_or(1.0).max(1.0) as i64;
    let quality = type_attribute(&*db, &all.iter().map(|p| p.type_id).collect::<Vec<_>>(), ATTR_HARVESTER_QUALITY).await;
    let to = |s: i64, t: i64| links.iter().find(|(src, ty, _)| *src == s && *ty == t).map(|(_, _, d)| *d).filter(|d| by_id.contains_key(d));
    let from = |d: i64, t: i64| links.iter().find(|(_, ty, dst)| *dst == d && *ty == t).map(|(s, _, _)| *s).filter(|s| by_id.contains_key(s));

    let mut txn = ItemTxn::begin(db).await?;
    // Harvesters fill their silos.
    for h in all.iter().filter(|p| p.group_id == GROUP_MOON_MINING) {
        let Some(t) = h.provision else { continue };
        let Some(&(_, batches)) = moon.iter().find(|(m, _)| *m == t) else { continue };
        let Some(silo) = to(h.id, t) else { continue };
        let take = batches.min(quality.get(&h.type_id).copied().unwrap_or(1.0) as i64);
        put(&mut txn, &by_id[&silo], t, units(t, take)).await?;
    }
    // Reactors run when every input is there and every output has room.
    'reactors: for r in all.iter().filter(|p| p.group_id == GROUP_REACTOR) {
        let Some((inputs, outputs)) = reactions.get(&r.id) else { continue };
        let mut sources = Vec::new();
        for &(t, q) in inputs {
            let Some(silo) = from(r.id, t) else { continue 'reactors };
            if held(&txn, silo, t).await? < units(t, q) {
                continue 'reactors;
            }
            sources.push((silo, t, units(t, q)));
        }
        let mut sinks = Vec::new();
        for &(t, q) in outputs {
            let Some(silo) = to(r.id, t) else { continue 'reactors };
            if room(&txn, &by_id[&silo], t).await? < units(t, q) {
                continue 'reactors;
            }
            sinks.push((silo, t, units(t, q)));
        }
        for (silo, t, n) in sources {
            take_from(&mut txn, silo, t, n).await?;
        }
        for (silo, t, n) in sinks {
            put(&mut txn, &by_id[&silo], t, n).await?;
        }
    }
    txn.commit().await?;
    Ok(())
}

/// How much of `t` a silo holds.
async fn held(txn: &ItemTxn<'_>, silo: i64, t: i64) -> Result<i64, tokio_postgres::Error> {
    let r = txn
        .tx()
        .query_one(r#"SELECT COALESCE(sum("quantity"), 0)::bigint FROM entity WHERE "locationID" = $1 AND "typeID" = $2"#, &[&(silo as i32), &(t as i32)])
        .await?;
    Ok(r.get(0))
}

/// How many more units of `t` a silo can take.
async fn room(txn: &ItemTxn<'_>, silo: &Producer, t: i64) -> Result<i64, tokio_postgres::Error> {
    let r = txn
        .tx()
        .query_one(
            r#"SELECT (SELECT COALESCE("capacity", 0) FROM "invTypes" WHERE "typeID" = $2)
                    - (SELECT COALESCE(sum(t."volume" * e."quantity"), 0) FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID" WHERE e."locationID" = $1),
                      (SELECT GREATEST("volume", 0.0001) FROM "invTypes" WHERE "typeID" = $3)"#,
            &[&(silo.id as i32), &(silo.type_id as i32), &(t as i32)],
        )
        .await?;
    let (free, volume) = (float(&r, 0).unwrap_or(0.0), float(&r, 1).unwrap_or(1.0));
    Ok((free / volume).floor().max(0.0) as i64)
}

/// Adds up to `n` of `t` to a silo, as much as fits, on the stack already there.
async fn put(txn: &mut ItemTxn<'_>, silo: &Producer, t: i64, n: i64) -> Result<(), tokio_postgres::Error> {
    if silo.group_id != GROUP_SILO || silo.provision != Some(t) {
        return Ok(());
    }
    let n = n.min(room(txn, silo, t).await?);
    if n <= 0 {
        return Ok(());
    }
    let stack = txn
        .tx()
        .query_opt(r#"SELECT "itemID", "quantity" FROM entity WHERE "locationID" = $1 AND "typeID" = $2 LIMIT 1"#, &[&(silo.id as i32), &(t as i32)])
        .await?;
    match stack {
        Some(r) => txn.set_quantity(int(&r, 0).unwrap_or(0), int(&r, 1).unwrap_or(0) + n).await,
        None => txn
            .create(NewItem { type_id: t, owner_id: silo.owner_id, location_id: silo.id, flag: 0, quantity: n, singleton: false })
            .await
            .map(|_| ()),
    }
}

/// Takes `n` of `t` out of a silo, smallest stacks first.
async fn take_from(txn: &mut ItemTxn<'_>, silo: i64, t: i64, mut n: i64) -> Result<(), tokio_postgres::Error> {
    let stacks = txn
        .tx()
        .query(r#"SELECT "itemID", "quantity" FROM entity WHERE "locationID" = $1 AND "typeID" = $2 ORDER BY "quantity""#, &[&(silo as i32), &(t as i32)])
        .await?;
    for r in stacks {
        if n == 0 {
            break;
        }
        let (id, q) = (int(&r, 0).unwrap_or(0), int(&r, 1).unwrap_or(0));
        let take = n.min(q);
        if take == q { txn.destroy(id).await? } else { txn.set_quantity(id, q - take).await? }
        n -= take;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Vec<(i64, f64)> {
        vec![(1, 2.0), (2, 2.0), (3, 8.0), (4, 16.0), (5, 64.0), (6, 256.0)]
    }

    #[test]
    fn a_moon_always_holds_the_same_materials() {
        assert_eq!(moon_materials(40_000_123, -0.2, &table()), moon_materials(40_000_123, -0.2, &table()));
    }

    #[test]
    fn the_rarest_materials_are_only_in_null_security() {
        for moon in 40_000_000..40_002_000 {
            assert!(!moon_materials(moon, 0.3, &table()).iter().any(|(t, _)| *t == 6), "moon {moon}");
        }
        assert!((40_000_000..40_002_000).any(|m| moon_materials(m, -0.5, &table()).iter().any(|(t, _)| *t == 6)));
    }
}
