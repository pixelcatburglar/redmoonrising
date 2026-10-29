//! Moving items between containers: the invcontainer writes Add, MultiAdd,
//! MergeItems, MultiMerge and StackAll (invCache.py:621-763), ReplaceCharges
//! (shipmodulebutton.py:311), and ship.AssembleShip (menusvc.py:2452).
//!
//! A character can reach its own items in the station it is docked in (on the
//! hangar floor, or one or two containers deep) or, in space, in its ship.

use eve_marshal::Value;
use tracing::{error, info};

use crate::dogma::category;
use crate::items::{self, DbResult, Item, ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::svc::fitting::{self, Snapshot};
use crate::svc::inventory::{Container, CONTAINER_HANGAR, FLAG_HANGAR};

pub const FLAG_CARGO: i64 = 5;
pub const FLAG_DRONE_BAY: i64 = 87;
const CATEGORY_SHIP: i64 = 6;
const ATTR_DRONE_CAPACITY: i32 = 283;

/// Why a move did not happen.
pub enum MoveError {
    Refused(String),
    Db(tokio_postgres::Error),
}

impl From<tokio_postgres::Error> for MoveError {
    fn from(e: tokio_postgres::Error) -> Self {
        MoveError::Db(e)
    }
}

pub fn refuse<T>(why: impl Into<String>) -> Result<T, MoveError> {
    Err(MoveError::Refused(why.into()))
}

/// The writes on a bound container; None for anything else.
pub async fn container_write(ctx: &Ctx<'_>, container: Container, call: &Call) -> Option<Outcome> {
    let flag = |i: usize| call.kwarg("flag").or(call.args.get(i)).and_then(|v| v.as_i64());
    let ids = |v: Option<&Value>| -> Vec<i64> {
        v.and_then(|v| v.seq()).unwrap_or(&[]).iter().filter_map(|v| v.as_i64()).collect()
    };
    let op = match call.method.as_str() {
        // Add(itemID, quantity=None, flag=None)
        "Add" => Op::Add {
            items: vec![call.arg_id(0)],
            quantity: call.kwarg("qty").or(call.args.get(1)).and_then(|v| v.as_i64()),
            flag: flag(2),
        },
        // MultiAdd(itemIDs, quantity, flag): always whole stacks.
        "MultiAdd" => Op::Add { items: ids(call.args.first()), quantity: None, flag: flag(2) },
        "MergeItems" => Op::Merge(vec![(call.arg_id(0), call.arg_id(1), call.arg_i64(2))]),
        // MultiMerge([(sourceID, destID, quantity)])
        "MultiMerge" => Op::Merge(
            call.args
                .first()
                .and_then(|v| v.seq())
                .unwrap_or(&[])
                .iter()
                .filter_map(|op| {
                    let op = op.seq()?;
                    Some((op.first()?.as_i64()?, op.get(1)?.as_i64()?, op.get(2).and_then(|q| q.as_i64())))
                })
                .collect(),
        ),
        // StackAll(flag=None, securityCode=None)
        "StackAll" => Op::StackAll { flag: flag(0) },
        // ReplaceCharges(flag, itemID, quantity): change ammo (shipmodulebutton.py:311).
        "ReplaceCharges" => Op::ReplaceCharges { flag: flag(0), item: call.arg_id(1), quantity: call.arg_i64(2) },
        _ => return None,
    };
    Some(run(ctx, container, op).await)
}

/// ship.AssembleShip(itemIDs): packaged ships in the hangar become usable ones.
pub async fn assemble_ship(ctx: &Ctx<'_>, item_ids: Vec<i64>) -> Outcome {
    let hangar = Container::Special { container_id: CONTAINER_HANGAR, owner_id: None };
    run(ctx, hangar, Op::Assemble(item_ids)).await
}

enum Op {
    Add { items: Vec<i64>, quantity: Option<i64>, flag: Option<i64> },
    Merge(Vec<(i64, i64, Option<i64>)>),
    StackAll { flag: Option<i64> },
    ReplaceCharges { flag: Option<i64>, item: i64, quantity: Option<i64> },
    Assemble(Vec<i64>),
}

/// Run `op` in one item transaction and report the result to the client.
async fn run(ctx: &Ctx<'_>, container: Container, op: Op) -> Outcome {
    // The loadout before any fitting change, to report what it did.
    let mut fitting_before: Option<Snapshot> = None;
    let result: Result<(Vec<Value>, Vec<i64>), MoveError> = async {
        let mut client = crate::db::connect(ctx.db_url).await?;
        let mut txn = ItemTxn::begin(&mut client).await?;
        apply(ctx, &mut txn, container, op, &mut fitting_before).await?;
        let origins = txn.origins();
        Ok((txn.commit().await?, origins))
    }
    .await;
    match result {
        Ok((events, origins)) => {
            items::notify(ctx.world, ctx.client_id, events);
            // Taking from another pilot's can is theft; taking the last thing
            // out of a can in space ends the can.
            if let Some(system) = ctx.session.solar_system_id {
                let corp_id = ctx.session.corp_id.unwrap_or(0);
                crate::concord::took_from(ctx.db, ctx.world, system, me(ctx), corp_id, &origins).await;
                crate::loot::clear_emptied(ctx.db, ctx.world, system, &origins).await;
            }
            if let Some(before) = fitting_before {
                fitting::push_attribute_changes(ctx, &before).await;
            }
            Outcome::Value(Value::None)
        }
        Err(MoveError::Refused(why)) => {
            info!("move refused: {why}");
            Outcome::custom(why)
        }
        Err(MoveError::Db(e)) => {
            error!("item move: {}", crate::logging::chain(&e));
            Outcome::custom("The items could not be moved.")
        }
    }
}

fn me(ctx: &Ctx<'_>) -> i64 {
    ctx.session.char_id.unwrap_or(0)
}

fn new_like(item: &Item) -> NewItem {
    NewItem {
        type_id: item.type_id,
        owner_id: item.owner_id,
        location_id: item.location_id,
        flag: item.flag,
        quantity: item.quantity,
        singleton: item.singleton,
    }
}

/// Where things put into a container go.
struct Destination {
    location: i64,
    flag: i64,
    /// m3, or None for unlimited.
    capacity: Option<f64>,
    /// The ship, when this is one of its slots.
    fitting: Option<Item>,
}

async fn apply(
    ctx: &Ctx<'_>,
    txn: &mut ItemTxn<'_>,
    container: Container,
    op: Op,
    fitting_before: &mut Option<Snapshot>,
) -> Result<(), MoveError> {
    match op {
        Op::Add { items, quantity, flag } => {
            let dest = destination(ctx, txn, container, flag).await?;
            for id in items {
                add(ctx, txn, &dest, id, quantity, fitting_before).await?;
            }
        }
        Op::Merge(ops) => {
            let dest = destination(ctx, txn, container, None).await?;
            for (source, target, quantity) in ops {
                merge(ctx, txn, &dest, source, target, quantity).await?;
            }
        }
        Op::StackAll { flag } => {
            let dest = destination(ctx, txn, container, flag).await?;
            stack_all(ctx, txn, &dest, flag.is_some()).await?;
        }
        Op::ReplaceCharges { flag, item, quantity } => {
            let dest = destination(ctx, txn, container, flag).await?;
            let Some(ship) = &dest.fitting else { return refuse("Charges can only be loaded into a module.") };
            if fitting_before.is_none() {
                *fitting_before = Some(fitting::snapshot(txn.tx(), ctx.dogma, me(ctx), ctx.session.ship_id).await?);
            }
            // The old charges go back to the hold, then the new ones load as a reload would.
            fitting::unload(txn, ship.id, dest.flag, ship.id, FLAG_CARGO).await?;
            add(ctx, txn, &dest, item, quantity, fitting_before).await?;
        }
        Op::Assemble(ids) => assemble(ctx, txn, &ids).await?,
    }
    Ok(())
}

async fn assemble(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_>, ids: &[i64]) -> Result<(), MoveError> {
    let Some(station) = ctx.session.station_id else { return refuse("You must be docked.") };
    for item in txn.items(ids).await? {
        if item.category_id != CATEGORY_SHIP || item.singleton {
            continue;
        }
        if item.owner_id != me(ctx) || item.location_id != station {
            return refuse("You can only assemble your own ships in your hangar.");
        }
        if item.quantity > 1 {
            txn.set_quantity(item.id, item.quantity - 1).await?;
            txn.create(NewItem { quantity: 1, singleton: true, ..new_like(&item) }).await?;
        } else {
            txn.set_singleton(item.id, true).await?;
        }
    }
    Ok(())
}

async fn destination(
    ctx: &Ctx<'_>,
    txn: &mut ItemTxn<'_>,
    container: Container,
    flag: Option<i64>,
) -> Result<Destination, MoveError> {
    match container {
        Container::Special { container_id: CONTAINER_HANGAR, owner_id } => {
            let Some(station) = ctx.session.station_id else { return refuse("You must be docked to use the hangar.") };
            if owner_id.is_some_and(|o| o != me(ctx)) {
                return refuse("You cannot move items into that hangar.");
            }
            Ok(Destination { location: station, flag: FLAG_HANGAR as i64, capacity: None, fitting: None })
        }
        Container::Special { .. } => refuse("Items cannot be moved there."),
        Container::Item(id) => {
            let Some(holder) = txn.item(id).await? else { return refuse("That container no longer exists.") };
            if !reachable(ctx, txn, &holder).await? {
                return refuse("That container is out of reach.");
            }
            if holder.category_id == CATEGORY_SHIP {
                let flag = flag.unwrap_or(FLAG_CARGO);
                if fitting::is_fitting_flag(flag) {
                    return Ok(Destination { location: id, flag, capacity: None, fitting: Some(holder) });
                }
                let capacity = match flag {
                    FLAG_CARGO => type_capacity(txn, holder.type_id).await?,
                    FLAG_DRONE_BAY => drone_capacity(txn, holder.type_id).await?,
                    _ => return refuse("Items cannot be put there."),
                };
                Ok(Destination { location: id, flag, capacity: Some(capacity), fitting: None })
            } else {
                if !holder.singleton {
                    return refuse("Assemble the container before putting things in it.");
                }
                let capacity = type_capacity(txn, holder.type_id).await?;
                if capacity <= 0.0 {
                    return refuse("That item cannot hold anything.");
                }
                Ok(Destination { location: id, flag: flag.unwrap_or(0), capacity: Some(capacity), fitting: None })
            }
        }
    }
}

async fn type_capacity(txn: &ItemTxn<'_>, type_id: i64) -> DbResult<f64> {
    let r = txn
        .tx()
        .query_opt(r#"SELECT "capacity" FROM "invTypes" WHERE "typeID" = $1"#, &[&(type_id as i32)])
        .await?;
    Ok(r.and_then(|r| r.get::<_, Option<f64>>(0)).unwrap_or(0.0))
}

async fn drone_capacity(txn: &ItemTxn<'_>, type_id: i64) -> DbResult<f64> {
    let r = txn
        .tx()
        .query_opt(
            r#"SELECT coalesce("valueFloat", "valueInt"::float8) FROM "dgmTypeAttributes"
                WHERE "typeID" = $1 AND "attributeID" = $2"#,
            &[&(type_id as i32), &ATTR_DRONE_CAPACITY],
        )
        .await?;
    Ok(r.and_then(|r| r.get::<_, Option<f64>>(0)).unwrap_or(0.0))
}

/// Volume already in a destination, leaving out `except`.
async fn used(txn: &ItemTxn<'_>, dest: &Destination, except: i64) -> DbResult<f64> {
    let r = txn
        .tx()
        .query_one(
            r#"SELECT coalesce(sum(t."volume" * e."quantity"), 0)::float8
                 FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID"
                WHERE e."locationID" = $1 AND e."flag" = $2 AND e."itemID" <> $3"#,
            &[&(dest.location as i32), &(dest.flag as i32), &(except as i32)],
        )
        .await?;
    Ok(r.get(0))
}

async fn check_room(txn: &ItemTxn<'_>, dest: &Destination, item: &Item, quantity: i64) -> Result<(), MoveError> {
    let Some(capacity) = dest.capacity else { return Ok(()) };
    let free = capacity - used(txn, dest, item.id).await?;
    let needed = item.volume * quantity as f64;
    if needed > free + 1e-6 {
        return refuse(format!("Not enough room: that needs {needed:.2} m3 and only {free:.2} m3 is free."));
    }
    Ok(())
}

/// Whether `item` sits in a can floating within reach of the caller's ship.
fn in_reachable_can(ctx: &Ctx<'_>, item: &Item) -> bool {
    match (ctx.session.solar_system_id, ctx.session.ship_id) {
        (Some(system), Some(ship)) => crate::loot::in_reach(ctx.world, system, ship, item.location_id),
        _ => false,
    }
}

/// Whether the caller can take `item`: its own, and here; or anything in a
/// can it is close to.
async fn reachable(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_>, item: &Item) -> DbResult<bool> {
    if in_reachable_can(ctx, item) {
        return Ok(true);
    }
    if item.owner_id != me(ctx) {
        return Ok(false);
    }
    let here = match (ctx.session.station_id, ctx.session.ship_id) {
        (Some(station), _) => station,
        (None, Some(ship)) => {
            if item.id == ship || item.location_id == ship {
                return Ok(true);
            }
            ship
        }
        (None, None) => return Ok(false),
    };
    // Up to two containers deep: a can in a ship's hold in the station.
    let mut location = item.location_id;
    for _ in 0..3 {
        if location == here {
            return Ok(true);
        }
        match txn.item(location).await? {
            Some(parent) if parent.owner_id == me(ctx) => location = parent.location_id,
            _ => return Ok(false),
        }
    }
    Ok(false)
}

async fn add(
    ctx: &Ctx<'_>,
    txn: &mut ItemTxn<'_>,
    dest: &Destination,
    id: i64,
    quantity: Option<i64>,
    fitting_before: &mut Option<Snapshot>,
) -> Result<(), MoveError> {
    let Some(item) = txn.item(id).await? else { return refuse("That item no longer exists.") };
    if !reachable(ctx, txn, &item).await? {
        return refuse("That item is out of reach.");
    }
    if item.location_id == dest.location && item.flag == dest.flag {
        return Ok(());
    }
    if item.id == dest.location {
        return refuse("An item cannot be put inside itself.");
    }
    if item.flag == 7 || item.id == me(ctx) {
        return refuse("That cannot be moved.");
    }
    if Some(item.id) == ctx.session.ship_id && dest.capacity.is_some() {
        return refuse("You cannot put your active ship in a container.");
    }
    if item.category_id == CATEGORY_SHIP && item.singleton && dest.capacity.is_some() {
        return refuse("Assembled ships cannot be put in a cargo hold; repackage it first.");
    }
    if (fitting::is_slot(item.flag) || dest.fitting.is_some()) && fitting_before.is_none() {
        *fitting_before = Some(fitting::snapshot(txn.tx(), ctx.dogma, me(ctx), ctx.session.ship_id).await?);
    }
    if let Some(ship) = &dest.fitting {
        return fitting::fit(ctx, txn, ship, &item, dest.flag, quantity).await;
    }
    if fitting::is_slot(item.flag) && item.category_id == category::MODULE {
        if ctx.session.station_id.is_none() {
            return refuse("Modules can only be unfitted while docked.");
        }
        fitting::unfit(txn, &item, dest.location, dest.flag).await?;
    }
    let quantity = if item.singleton { 1 } else { quantity.unwrap_or(item.quantity).clamp(1, item.quantity) };
    check_room(txn, dest, &item, quantity).await?;
    // Loot taken from a can becomes the taker's.
    let owner = me(ctx);
    if quantity < item.quantity {
        txn.set_quantity(item.id, item.quantity - quantity).await?;
        let new = NewItem { location_id: dest.location, flag: dest.flag, quantity, singleton: false, owner_id: owner, ..new_like(&item) };
        txn.create(new).await?;
    } else {
        txn.move_to(item.id, dest.location, dest.flag).await?;
        if item.owner_id != owner {
            txn.set_owner(item.id, owner).await?;
        }
    }
    Ok(())
}

async fn merge(
    ctx: &Ctx<'_>,
    txn: &mut ItemTxn<'_>,
    dest: &Destination,
    source: i64,
    target: i64,
    quantity: Option<i64>,
) -> Result<(), MoveError> {
    let (Some(from), Some(onto)) = (txn.item(source).await?, txn.item(target).await?) else {
        return refuse("That item no longer exists.");
    };
    if !reachable(ctx, txn, &from).await? || !reachable(ctx, txn, &onto).await? {
        return refuse("That item is out of reach.");
    }
    if from.id == onto.id {
        return Ok(());
    }
    if from.type_id != onto.type_id || from.singleton || onto.singleton {
        return refuse("Only stacks of the same kind can be merged.");
    }
    let quantity = quantity.unwrap_or(from.quantity).clamp(1, from.quantity);
    if from.location_id != onto.location_id || from.flag != onto.flag {
        check_room(txn, dest, &from, quantity).await?;
    }
    txn.set_quantity(onto.id, onto.quantity + quantity).await?;
    txn.set_quantity(from.id, from.quantity - quantity).await?;
    Ok(())
}

/// Merge every stack of a type in the destination into its oldest stack.
async fn stack_all(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_>, dest: &Destination, one_flag: bool) -> Result<(), MoveError> {
    let rows = txn
        .tx()
        .query(
            r#"SELECT "itemID" FROM entity
                WHERE "locationID" = $1 AND "ownerID" = $2 AND "singleton" = 0
                  AND ($3 = false OR "flag" = $4)
                ORDER BY "typeID", "flag", "itemID""#,
            &[&(dest.location as i32), &(me(ctx) as i32), &one_flag, &(dest.flag as i32)],
        )
        .await?;
    let ids: Vec<i64> = rows.iter().map(|r| r.get::<_, i32>(0) as i64).collect();
    let mut keep: Option<Item> = None;
    for item in txn.items(&ids).await? {
        match &mut keep {
            Some(k) if k.type_id == item.type_id && k.flag == item.flag => {
                k.quantity += item.quantity;
                txn.set_quantity(k.id, k.quantity).await?;
                txn.destroy(item.id).await?;
            }
            _ => keep = Some(item),
        }
    }
    Ok(())
}
