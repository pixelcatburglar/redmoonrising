//! What is left floating after a fight: cargo containers holding a rat's loot
//! or what survived of a player's ship.
//!
//! A can is a real item in the solar system (with its position in the entity
//! row), so it outlives a restart; its contents are ordinary items inside it,
//! opened and emptied through the usual inventory calls. Anyone close enough
//! may take from it, and what they take becomes theirs. A can goes when it is
//! empty or two hours after it was dropped.
//!
//! Rat loot is drawn from the market's modules and charges, priced to the
//! rat's bounty.

use std::sync::OnceLock;

use eve_marshal::Value;
use tokio_postgres::Client;
use tracing::{error, info};

use crate::ballpark::{Park, Vec3};
use crate::combat::Rng;
use crate::heartbeat::Out;
use crate::items::{ItemTxn, NewItem};
use crate::svc::space::{self, entry};
use crate::world::World;

pub const TYPE_CARGO_CONTAINER: i64 = 23;
const GROUP_CARGO_CONTAINER: i32 = 12;
/// How close a ship must be to take from a can (const.maxCargoContainerTransferDistance).
pub const TRANSFER_RANGE: f64 = 2_500.0;
const LIFETIME_MS: i64 = 2 * 3_600_000;
const CATEGORY_MODULE: i32 = 7;
const CATEGORY_CHARGE: i32 = 8;

#[derive(Debug, Clone)]
pub struct Can {
    pub id: i64,
    pub type_id: i64,
    pub name: String,
    pub pos: Vec3,
    pub radius: f64,
    pub owner_id: i64,
    pub expires_ms: i64,
}

/// What goes into a new can: something new, or an item that already exists.
#[derive(Debug, Clone)]
pub enum Stash {
    New { type_id: i64, quantity: i64 },
    Existing(i64),
}

pub fn slim(can: &Can) -> Value {
    space::slim(vec![
        ("itemID", Value::Int(can.id)),
        ("typeID", Value::Int(can.type_id)),
        ("ownerID", Value::Int(can.owner_id)),
    ])
}

// ---- loot tables -------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct LootType {
    type_id: i64,
    price: f64,
    charge: bool,
    portion: i64,
}

static TABLE: OnceLock<Vec<LootType>> = OnceLock::new();

/// Reads what rats can drop, once.
pub async fn ensure_table(db: &tokio_postgres::Client) -> Result<(), tokio_postgres::Error> {
    if TABLE.get().is_some() {
        return Ok(());
    }
    let rows = db
        .query(
            r#"SELECT t."typeID", t."basePrice", g."categoryID", COALESCE(t."portionSize", 1)
                 FROM "invTypes" t JOIN "invGroups" g ON g."groupID" = t."groupID"
                WHERE g."categoryID" = ANY($1) AND t."marketGroupID" IS NOT NULL AND t."basePrice" > 0
                ORDER BY t."basePrice""#,
            &[&vec![CATEGORY_MODULE, CATEGORY_CHARGE]],
        )
        .await?;
    let table = rows
        .iter()
        .map(|r| LootType {
            type_id: r.get::<_, i32>(0) as i64,
            price: r.get(1),
            charge: r.get::<_, i32>(2) == CATEGORY_CHARGE,
            portion: (r.get::<_, i32>(3) as i64).max(1),
        })
        .collect::<Vec<_>>();
    info!("loot table: {} types", table.len());
    let _ = TABLE.set(table);
    Ok(())
}

/// What a rat worth `bounty` drops: some modules and charges worth about
/// what it was, or nothing at all.
pub fn roll(bounty: f64, count: (usize, usize), rng: &mut Rng) -> Vec<Stash> {
    let Some(table) = TABLE.get() else { return Vec::new() };
    if rng.unit() < 0.3 {
        return Vec::new();
    }
    let (lo, hi) = if count.1 > 0 { (count.0.max(1), count.1.max(count.0.max(1))) } else { (1, 2) };
    let n = lo + rng.below(hi - lo + 1);
    let affordable = |t: &&LootType| {
        if t.charge { t.price * 100.0 <= bounty.max(1_000.0) } else { t.price <= bounty.max(1_000.0) * 1.5 }
    };
    let pool: Vec<&LootType> = table.iter().filter(affordable).collect();
    if pool.is_empty() {
        return Vec::new();
    }
    (0..n)
        .map(|_| {
            // Cheaper things are commoner: the better of two picks from the bottom.
            let t = pool[rng.below(pool.len()).min(rng.below(pool.len()))];
            let quantity = if t.charge { t.portion * (20 + rng.below(81) as i64) } else { 1 };
            Stash::New { type_id: t.type_id, quantity }
        })
        .collect()
}

// ---- cans in the park --------------------------------------------------------------------

/// The cans already floating in a system, for a new park.
pub async fn load_cans(db: &tokio_postgres::Client, solar_system_id: i64, now: i64) -> Result<Vec<Can>, tokio_postgres::Error> {
    let rows = db
        .query(
            r#"SELECT e."itemID", e."typeID", COALESCE(NULLIF(e."itemName", ''), t."typeName", ''),
                      e."x", e."y", e."z", COALESCE(t."radius", 10), e."ownerID"
                 FROM entity e
                 JOIN "invTypes" t ON t."typeID" = e."typeID"
                WHERE e."locationID" = $1 AND t."groupID" = $2"#,
            &[&(solar_system_id as i32), &GROUP_CARGO_CONTAINER],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| Can {
            id: r.get::<_, i32>(0) as i64,
            type_id: r.get::<_, i32>(1) as i64,
            name: r.get(2),
            pos: Vec3::new(r.get(3), r.get(4), r.get(5)),
            radius: r.get(6),
            owner_id: r.get::<_, i32>(7) as i64,
            expires_ms: now + LIFETIME_MS,
        })
        .collect())
}

/// Cans past their time go, and their contents with them.
pub fn expire(park: &mut Park, now: i64, out: &mut Out) {
    let old: Vec<i64> = park.cans.values().filter(|c| c.expires_ms <= now).map(|c| c.id).collect();
    for id in old {
        park.cans.remove(&id);
        out.destiny(park.solar_system_id, entry(park.tick, "RemoveBalls", vec![Value::List(vec![Value::Int(id)])]));
        out.jobs.push(crate::heartbeat::Job::Scrap { can_id: id });
    }
}

/// Whether `ship_id` is close enough to `can_id` to take from it.
pub fn in_reach(world: &World, solar_system_id: i64, ship_id: i64, can_id: i64) -> bool {
    let w = world.lock();
    let Some(park) = w.parks.get(&solar_system_id) else { return false };
    let (Some(can), Some(ship)) = (park.cans.get(&can_id), park.ships.get(&ship_id)) else { return false };
    (can.pos - ship.pos).len() - can.radius - ship.stats.radius <= TRANSFER_RANGE * 1.25
}

pub fn is_can(world: &World, solar_system_id: i64, item_id: i64) -> bool {
    world.lock().parks.get(&solar_system_id).is_some_and(|p| p.cans.contains_key(&item_id))
}

// ---- dropping and clearing -------------------------------------------------------------------

/// Puts a can at `pos` holding `stash`, owned by `owner_id`, and shows it to
/// everyone in the system.
pub async fn drop_can(db: &mut Client, world: &World, solar_system_id: i64, pos: Vec3, owner_id: i64, stash: Vec<Stash>) {
    if stash.is_empty() {
        return;
    }
    let result = async {
        let mut txn = ItemTxn::begin(db).await?;
        let can = txn
            .create(NewItem {
                type_id: TYPE_CARGO_CONTAINER,
                owner_id,
                location_id: solar_system_id,
                flag: 0,
                quantity: 1,
                singleton: true,
            })
            .await?;
        txn.set_position(can, pos.tuple()).await?;
        for s in &stash {
            match *s {
                Stash::New { type_id, quantity } => {
                    let singleton = false;
                    txn.create(NewItem { type_id, owner_id, location_id: can, flag: 0, quantity, singleton }).await?;
                }
                Stash::Existing(id) => txn.move_to(id, can, 0).await?,
            }
        }
        let name: String = txn
            .tx()
            .query_one(r#"SELECT COALESCE("typeName", '') FROM "invTypes" WHERE "typeID" = $1"#, &[&(TYPE_CARGO_CONTAINER as i32)])
            .await
            .map(|r| r.get(0))?;
        let events = txn.commit().await?;
        Ok::<_, tokio_postgres::Error>((can, name, events))
    }
    .await;
    let (can_id, name, events) = match result {
        Ok(v) => v,
        Err(e) => {
            error!("dropping a can in {solar_system_id}: {}", crate::logging::chain(&e));
            return;
        }
    };
    let mut w = world.lock();
    // The owner sees its items leave a ship it no longer has.
    for client in w.clients_of(owner_id) {
        for event in &events {
            w.notify(&[client], "OnItemChange", event.clone());
        }
    }
    let Some(park) = w.parks.get_mut(&solar_system_id) else { return };
    let can = Can { id: can_id, type_id: TYPE_CARGO_CONTAINER, name, pos, radius: 14.0, owner_id, expires_ms: crate::combat::now_ms() + LIFETIME_MS };
    let blob = crate::destiny::encode(crate::destiny::stamp(park.tick), &[crate::destiny::Ball::celestial(can.id, can.radius, can.pos.tuple(), &can.name, false)]);
    let chunk = Value::Tuple(vec![Value::Bytes(blob), Value::List(vec![slim(&can)]), Value::Dict(Vec::new())]);
    let add = entry(park.tick, "AddBalls", vec![chunk]);
    park.cans.insert(can_id, can);
    space::broadcast(&w, solar_system_id, None, vec![add]);
    info!("can {can_id} dropped in {solar_system_id} with {} stack(s)", stash.len());
}

/// After items left some containers: any of them that is a can in space and
/// now empty goes.
pub async fn clear_emptied(db: &tokio_postgres::Client, world: &World, solar_system_id: i64, origins: &[i64]) {
    let cans: Vec<i64> = origins.iter().copied().filter(|id| is_can(world, solar_system_id, *id)).collect();
    for can in cans {
        let left: i64 = match db.query_one(r#"SELECT count(*) FROM entity WHERE "locationID" = $1"#, &[&(can as i32)]).await {
            Ok(r) => r.get(0),
            Err(e) => {
                error!("counting the contents of can {can}: {}", crate::logging::chain(&e));
                continue;
            }
        };
        if left > 0 {
            continue;
        }
        if let Err(e) = db.execute(r#"DELETE FROM entity WHERE "itemID" = $1"#, &[&(can as i32)]).await {
            error!("removing empty can {can}: {}", crate::logging::chain(&e));
            continue;
        }
        let mut w = world.lock();
        if let Some(park) = w.parks.get_mut(&solar_system_id) {
            park.cans.remove(&can);
            let gone = entry(park.tick, "RemoveBalls", vec![Value::List(vec![Value::Int(can)])]);
            space::broadcast(&w, solar_system_id, None, vec![gone]);
        }
        info!("can {can} emptied and gone");
    }
}

/// An expired can and everything in it.
pub async fn scrap(db: &mut Client, can_id: i64) {
    let result = async {
        let mut txn = ItemTxn::begin(db).await?;
        let inside: Vec<i64> = txn
            .tx()
            .query(r#"SELECT "itemID" FROM entity WHERE "locationID" = $1"#, &[&(can_id as i32)])
            .await?
            .iter()
            .map(|r| r.get::<_, i32>(0) as i64)
            .collect();
        for id in inside {
            txn.destroy(id).await?;
        }
        txn.destroy(can_id).await?;
        txn.commit().await
    }
    .await;
    if let Err(e) = result {
        error!("scrapping can {can_id}: {}", crate::logging::chain(&e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolls_respect_the_bounty() {
        let _ = TABLE.set(vec![
            LootType { type_id: 1, price: 500.0, charge: true, portion: 1 },
            LootType { type_id: 2, price: 5_000.0, charge: false, portion: 1 },
            LootType { type_id: 3, price: 5_000_000.0, charge: false, portion: 1 },
        ]);
        let mut rng = Rng::seeded(3);
        for _ in 0..200 {
            for s in roll(10_000.0, (1, 3), &mut rng) {
                let Stash::New { type_id, quantity } = s else { panic!() };
                assert_ne!(type_id, 3, "too dear for a 10k rat");
                assert!(type_id != 2 || quantity == 1);
            }
        }
    }
}
