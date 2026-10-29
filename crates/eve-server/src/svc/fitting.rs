//! Fitting: modules into slots, charges into modules, and modules on- and
//! offline, with the ship's numbers recomputed by the dogma engine.
//!
//! A module goes into the slot it asks for or, with flagAutoFit, the first free
//! one of its kind; the ship must have that many slots and, for turrets and
//! launchers, a free hardpoint. A fitted module goes online if the ship has the
//! CPU and power grid left for it. Taking a module out takes it offline and
//! brings its charge along.
//!
//! The client hears item moves through OnItemChange; what that does to the
//! ship's and modules' attributes goes out as OnModuleAttributeChanges, and a
//! module going on- or offline as OnGodmaShipEffect (godma.py:777, :800).

use std::collections::{HashMap, HashSet};

use eve_marshal::Value;
use tracing::{error, info};

use crate::dogma::{attr, category, effect, Catalog, Loadout, State};
use crate::items::{Item, ItemTxn, NewItem};
use crate::services::{Ctx, Outcome};
use crate::svc::moves::{refuse, MoveError};

const FLAG_AUTO_FIT: i64 = 0;
pub const EFFECT_ONLINE: i64 = 16;
/// Module attributes naming the charge groups it takes, and its charge size.
const CHARGE_GROUPS: [i32; 5] = [604, 605, 606, 609, 610];
const CHARGE_SIZE: i32 = 128;

/// Each kind of slot: the module effect that asks for it, its first flag and
/// the ship attribute that counts them.
const SLOTS: [(i32, i64, i32); 4] = [
    (effect::LO_POWER, 11, attr::LOW_SLOTS),
    (effect::MED_POWER, 19, attr::MED_SLOTS),
    (effect::HI_POWER, 27, attr::HI_SLOTS),
    (effect::RIG_SLOT, 92, attr::RIG_SLOTS),
];

/// Every computed attribute of a loadout, by item.
pub type Snapshot = HashMap<i64, HashMap<i32, f64>>;

pub async fn snapshot(db: &impl tokio_postgres::GenericClient, catalog: &Catalog, char_id: i64, ship_id: Option<i64>) -> Result<Snapshot, tokio_postgres::Error> {
    Ok(Loadout::load(db, catalog, char_id, ship_id, &HashSet::new()).await?.snapshot())
}

pub fn is_slot(flag: i64) -> bool {
    (11..=35).contains(&flag) || (92..=99).contains(&flag)
}

pub fn is_fitting_flag(flag: i64) -> bool {
    flag == FLAG_AUTO_FIT || is_slot(flag)
}

/// Put `item` (a module, or up to `quantity` of a charge) into `ship` at `flag`.
pub async fn fit(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_>, ship: &Item, item: &Item, flag: i64, quantity: Option<i64>) -> Result<(), MoveError> {
    // Reloading works anywhere; changing modules needs a station.
    if ctx.session.station_id.is_none() && item.category_id != category::CHARGE {
        return refuse("Ships can only be refitted while docked.");
    }
    if Some(ship.id) != ctx.session.ship_id {
        return refuse("You can only fit your active ship.");
    }
    match item.category_id {
        category::CHARGE => load_charge(ctx, txn, ship, item, flag, quantity.unwrap_or(item.quantity)).await,
        category::MODULE => fit_module(ctx, txn, ship, item, flag).await,
        _ => refuse("Only modules and charges can be fitted."),
    }
}

async fn fit_module(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_>, ship: &Item, module: &Item, wanted: i64) -> Result<(), MoveError> {
    let char_id = ctx.session.char_id.unwrap_or(0);
    let loadout = Loadout::load(txn.tx(), ctx.dogma, char_id, Some(ship.id), &HashSet::new()).await?;
    let Some(ship_index) = loadout.find(ship.id) else { return refuse("Your ship could not be read.") };

    let effects: HashSet<i32> = txn
        .tx()
        .query(r#"SELECT "effectID" FROM "dgmTypeEffects" WHERE "typeID" = $1"#, &[&(module.type_id as i32)])
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let Some(&(_, first, count_attr)) = SLOTS.iter().find(|(e, _, _)| effects.contains(e)) else {
        return refuse("That module has no slot to go in.");
    };
    let slots = loadout.value(ship_index, count_attr).max(0.0) as i64;
    let taken: HashSet<i64> = loadout
        .pieces
        .iter()
        .filter(|p| p.category_id == category::MODULE)
        .map(|p| p.flag)
        .collect();
    let range = first..first + slots;
    let flag = if wanted == FLAG_AUTO_FIT {
        match range.clone().find(|f| !taken.contains(f)) {
            Some(f) => f,
            None => return refuse("There is no free slot of that kind."),
        }
    } else if !range.contains(&wanted) {
        return refuse("That module does not fit in that slot.");
    } else {
        wanted
    };
    // Dropped on a fitted slot: the module there goes back to the hangar.
    let occupant = loadout.pieces.iter().find(|p| p.category_id == category::MODULE && p.flag == flag);
    let loadout = match occupant.map(|p| p.item_id) {
        Some(occupant) => {
            let station = ctx.session.station_id.unwrap_or(0);
            let Some(old) = txn.item(occupant).await? else { return refuse("Your ship could not be read.") };
            unfit(txn, &old, station, crate::svc::inventory::FLAG_HANGAR.into()).await?;
            txn.move_to(old.id, station, crate::svc::inventory::FLAG_HANGAR.into()).await?;
            Loadout::load(txn.tx(), ctx.dogma, char_id, Some(ship.id), &HashSet::new()).await?
        }
        None => loadout,
    };
    let ship_index = ship_index_of(&loadout, ship.id);
    for (fitted, left, what) in [
        (effect::TURRET_FITTED, attr::TURRET_SLOTS_LEFT, "turret"),
        (effect::LAUNCHER_FITTED, attr::LAUNCHER_SLOTS_LEFT, "launcher"),
    ] {
        if effects.contains(&fitted) && loadout.value(ship_index, left) < 1.0 {
            return refuse(format!("There is no free {what} hardpoint."));
        }
    }

    let module_id = if module.quantity > 1 {
        txn.set_quantity(module.id, module.quantity - 1).await?;
        let new = NewItem {
            type_id: module.type_id,
            owner_id: module.owner_id,
            location_id: ship.id,
            flag,
            quantity: 1,
            singleton: true,
        };
        txn.create(new).await?
    } else {
        txn.move_to(module.id, ship.id, flag).await?;
        txn.set_singleton(module.id, true).await?;
        module.id
    };

    // Online straight away, if the ship can carry it.
    let loadout = Loadout::load(txn.tx(), ctx.dogma, char_id, Some(ship.id), &HashSet::new()).await?;
    if let Some(i) = loadout.find(module_id) {
        if can_online(&loadout, ship_index_of(&loadout, ship.id), i) {
            set_online_attr(txn, module_id, true).await?;
        }
    }
    info!("fitted module {module_id} (type {}) in slot {flag}", module.type_id);
    Ok(())
}

fn ship_index_of(loadout: &Loadout<'_>, ship_id: i64) -> usize {
    loadout.find(ship_id).unwrap_or(0)
}

/// Whether module `i` fits in the CPU and power grid the ship has left.
fn can_online(loadout: &Loadout<'_>, ship: usize, i: usize) -> bool {
    let cpu_left = loadout.value(ship, attr::CPU_OUTPUT) - loadout.value(ship, attr::CPU_LOAD);
    let power_left = loadout.value(ship, attr::POWER_OUTPUT) - loadout.value(ship, attr::POWER_LOAD);
    let (cpu, power) = (loadout.value(i, attr::CPU), loadout.value(i, attr::POWER));
    let online_now = loadout.pieces[i].state != State::Offline;
    online_now || (cpu <= cpu_left + 1e-6 && power <= power_left + 1e-6)
}

async fn set_online_attr(txn: &ItemTxn<'_>, item_id: i64, on: bool) -> Result<(), tokio_postgres::Error> {
    if on {
        txn.tx()
            .execute(
                r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt","valueFloat")
                   VALUES ($1, $2, 1, NULL)
                   ON CONFLICT ("itemID","attributeID") DO UPDATE SET "valueInt" = 1, "valueFloat" = NULL"#,
                &[&(item_id as i32), &attr::IS_ONLINE],
            )
            .await?;
    } else {
        txn.tx()
            .execute(
                r#"DELETE FROM entity_attributes WHERE "itemID" = $1 AND "attributeID" = $2"#,
                &[&(item_id as i32), &attr::IS_ONLINE],
            )
            .await?;
    }
    Ok(())
}

/// Load up to `wanted` of a charge into the module at `flag`, as far as the
/// module holds.
async fn load_charge(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_>, ship: &Item, charge: &Item, flag: i64, wanted: i64) -> Result<(), MoveError> {
    let char_id = ctx.session.char_id.unwrap_or(0);
    let loadout = Loadout::load(txn.tx(), ctx.dogma, char_id, Some(ship.id), &HashSet::new()).await?;
    let Some(module) = loadout.pieces.iter().position(|p| p.category_id == category::MODULE && p.flag == flag) else {
        return refuse("There is no module in that slot to load.");
    };
    let groups: Vec<i64> = CHARGE_GROUPS
        .iter()
        .map(|a| loadout.value(module, *a) as i64)
        .filter(|g| *g > 0)
        .collect();
    if !groups.contains(&charge_group(txn, charge.type_id).await?) {
        return refuse("That charge does not fit that module.");
    }
    let size = loadout.value(module, CHARGE_SIZE);
    if size > 0.0 {
        let charge_size = type_attribute(txn, charge.type_id, CHARGE_SIZE).await?;
        if (charge_size - size).abs() > 1e-6 {
            return refuse("That charge is the wrong size for that module.");
        }
    }
    let loaded = loadout.pieces.iter().find(|p| p.category_id == category::CHARGE && p.flag == flag);
    if let Some(loaded) = loaded {
        if loaded.type_id != charge.type_id {
            return refuse("Unload the current charges first.");
        }
    }
    let capacity = type_capacity(txn, loadout.pieces[module].type_id).await?;
    let room = if charge.volume > 0.0 { (capacity / charge.volume).floor() as i64 } else { charge.quantity };
    let have = match loaded {
        Some(l) => txn.item(l.item_id).await?.map_or(0, |i| i.quantity),
        None => 0,
    };
    let quantity = wanted.clamp(1, charge.quantity).min(room - have);
    if quantity <= 0 {
        return refuse("That module is already full.");
    }
    match loaded {
        Some(l) => txn.set_quantity(l.item_id, have + quantity).await?,
        None if quantity == charge.quantity => txn.move_to(charge.id, ship.id, flag).await?,
        None => {
            let new = NewItem { type_id: charge.type_id, owner_id: charge.owner_id, location_id: ship.id, flag, quantity, singleton: false };
            txn.create(new).await?;
        }
    }
    if !(loaded.is_none() && quantity == charge.quantity) {
        txn.set_quantity(charge.id, charge.quantity - quantity).await?;
    }
    Ok(())
}

async fn charge_group(txn: &ItemTxn<'_>, type_id: i64) -> Result<i64, tokio_postgres::Error> {
    let r = txn.tx().query_one(r#"SELECT "groupID" FROM "invTypes" WHERE "typeID" = $1"#, &[&(type_id as i32)]).await?;
    Ok(r.get::<_, i32>(0) as i64)
}

async fn type_capacity(txn: &ItemTxn<'_>, type_id: i64) -> Result<f64, tokio_postgres::Error> {
    let r = txn.tx().query_opt(r#"SELECT "capacity" FROM "invTypes" WHERE "typeID" = $1"#, &[&(type_id as i32)]).await?;
    Ok(r.and_then(|r| r.get::<_, Option<f64>>(0)).unwrap_or(0.0))
}

async fn type_attribute(txn: &ItemTxn<'_>, type_id: i64, attribute: i32) -> Result<f64, tokio_postgres::Error> {
    let r = txn
        .tx()
        .query_opt(
            r#"SELECT coalesce("valueFloat", "valueInt"::float8) FROM "dgmTypeAttributes"
                WHERE "typeID" = $1 AND "attributeID" = $2"#,
            &[&(type_id as i32), &attribute],
        )
        .await?;
    Ok(r.and_then(|r| r.get::<_, Option<f64>>(0)).unwrap_or(0.0))
}

/// A module leaving its slot goes offline and takes its charge with it.
pub async fn unfit(txn: &mut ItemTxn<'_>, module: &Item, location: i64, flag: i64) -> Result<(), MoveError> {
    set_online_attr(txn, module.id, false).await?;
    unload(txn, module.location_id, module.flag, location, flag).await
}

/// Move the charges in `ship`'s slot `slot` to `location` at `flag`.
pub async fn unload(txn: &mut ItemTxn<'_>, ship_id: i64, slot: i64, location: i64, flag: i64) -> Result<(), MoveError> {
    let charges = txn
        .tx()
        .query(
            r#"SELECT e."itemID" FROM entity e
                 JOIN "invTypes" t ON t."typeID" = e."typeID"
                 JOIN "invGroups" g ON g."groupID" = t."groupID"
                WHERE e."locationID" = $1 AND e."flag" = $2 AND g."categoryID" = $3"#,
            &[&(ship_id as i32), &(slot as i32), &(category::CHARGE as i32)],
        )
        .await?;
    for r in &charges {
        txn.move_to(r.get::<_, i32>(0) as i64, location, flag).await?;
    }
    Ok(())
}

/// OnModuleAttributeChanges for what differs between two snapshots, for items
/// in both (new items are read whole by the client).
pub fn attribute_changes(owner: i64, before: &Snapshot, after: &Snapshot) -> Vec<Value> {
    let now = crate::bulkdata::filetime_now();
    let mut out = Vec::new();
    for (item, attrs) in after {
        let Some(old) = before.get(item) else { continue };
        for (a, v) in attrs {
            let was = old.get(a).copied().unwrap_or(0.0);
            if (v - was).abs() > 1e-9 {
                out.push(Value::Tuple(vec![
                    Value::bytes("OnModuleAttributeChange"),
                    Value::Int(owner),
                    Value::Int(*item),
                    Value::Int(*a as i64),
                    Value::Long(now),
                    number(*v),
                    number(was),
                ]));
            }
        }
    }
    out
}

fn number(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.0e15 {
        Value::Int(v as i64)
    } else {
        Value::Double(v)
    }
}

/// Tell the client what changed between `before` and the loadout now.
pub async fn push_attribute_changes(ctx: &Ctx<'_>, before: &Snapshot) {
    let Some(char_id) = ctx.session.char_id else { return };
    match snapshot(ctx.db, ctx.dogma, char_id, ctx.session.ship_id).await {
        Ok(after) => {
            let changes = attribute_changes(char_id, before, &after);
            if !changes.is_empty() {
                ctx.world.lock().notify(&[ctx.client_id], "OnModuleAttributeChanges", Value::Tuple(vec![Value::List(changes)]));
            }
        }
        Err(e) => error!("recomputing attributes: {}", crate::logging::chain(&e)),
    }
}

/// dogmaIM Activate/Deactivate(itemID, "online"): put a fitted module on- or
/// offline.
pub async fn set_online(ctx: &Ctx<'_>, item_id: i64, on: bool) -> Outcome {
    let (Some(char_id), Some(ship_id)) = (ctx.session.char_id, ctx.session.ship_id) else {
        return Outcome::custom("You have no ship.");
    };
    let result: Result<Option<Snapshot>, MoveError> = async {
        let mut client = crate::db::connect(ctx.db_url).await?;
        let txn = ItemTxn::begin(&mut client).await?;
        let loadout = Loadout::load(txn.tx(), ctx.dogma, char_id, Some(ship_id), &HashSet::new()).await?;
        let Some(i) = loadout.find(item_id).filter(|i| loadout.pieces[*i].category_id == category::MODULE) else {
            return refuse("That module is not fitted to your ship.");
        };
        let online = loadout.pieces[i].state != State::Offline;
        if online == on {
            return Ok(None);
        }
        if on && !can_online(&loadout, ship_index_of(&loadout, ship_id), i) {
            return refuse("Your ship does not have the CPU or power grid left to online that module.");
        }
        let before = loadout.snapshot();
        set_online_attr(&txn, item_id, on).await?;
        txn.commit().await?;
        Ok(Some(before))
    }
    .await;
    match result {
        Ok(Some(before)) => {
            push_attribute_changes(ctx, &before).await;
            ctx.world.lock().notify(&[ctx.client_id], "OnGodmaShipEffect", online_effect(item_id, char_id, ship_id, on));
            info!("module {item_id} {}", if on { "online" } else { "offline" });
            Outcome::Value(Value::None)
        }
        Ok(None) => Outcome::Value(Value::None),
        Err(MoveError::Refused(why)) => Outcome::custom(why),
        Err(MoveError::Db(e)) => {
            error!("online toggle for {item_id}: {}", crate::logging::chain(&e));
            Outcome::custom("The module could not be changed.")
        }
    }
}

/// OnGodmaShipEffect(itemID, effectID, time, start, active, environment,
/// startTime, duration, repeat, randomSeed, error) for the online effect.
fn online_effect(item_id: i64, char_id: i64, ship_id: i64, on: bool) -> Value {
    let now = crate::bulkdata::filetime_now();
    let environment = Value::Tuple(vec![
        Value::Int(item_id),
        Value::Int(char_id),
        Value::Int(ship_id),
        Value::None,
        Value::None,
        Value::List(Vec::new()),
        Value::Int(EFFECT_ONLINE),
    ]);
    Value::Tuple(vec![
        Value::Int(item_id),
        Value::Int(EFFECT_ONLINE),
        Value::Long(now),
        Value::Int(i64::from(on)),
        Value::Int(i64::from(on)),
        environment,
        Value::Long(now),
        Value::Int(-1),
        Value::Int(-1),
        Value::None,
        Value::None,
    ])
}
