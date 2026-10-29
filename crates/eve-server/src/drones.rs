//! Drones: launched from a ship's drone bay into space (ship.Drop), told what
//! to do through the solar system's `entity` object (menusvc.py: CmdEngage,
//! CmdMine, CmdReturnHome, CmdReturnBay...), and scooped back (ScoopDrone or
//! by flying home).
//!
//! In space a drone is its own item in the solar system and a ship in the
//! park, with a [`Drone`] instead of a pilot. It fights with the guns its type
//! has, as rats do, and mines into its controller's cargo hold. Its pilot
//! hears what it is doing as OnDroneStateChange, with the activity states of
//! drone.py (idle, fighting, mining, returning...).

use std::collections::HashMap;

use eve_marshal::Value;
use tokio_postgres::Client;
use tracing::{debug, error, info, warn};

use crate::ballpark::{Command, Park, Pilot, Ship, ShipStats, Vec3};
use crate::combat::{self, attr, Action, Active, Lock, REPEAT_FOREVER};
use crate::conn::Server;
use crate::heartbeat::{Job, Out};
use crate::items::{self, ItemTxn, NewItem};
use crate::npc;
use crate::services::{Call, Ctx, Outcome};
use crate::svc::space;
use crate::world::World;

const FLAG_DRONE_BAY: i64 = 87;
const CATEGORY_DRONE: i64 = 18;
const EFFECT_MINING: i64 = 17;
/// A drone this close to its ship is back in the bay.
const DOCK_RANGE: f64 = 1_500.0;
/// How far from its ship it waits.
const IDLE_ORBIT: f64 = 1_000.0;
/// How far a pilot reaches to scoop a drone (const.maxCargoContainerTransferDistance).
const SCOOP_RANGE: f64 = 2_500.0;
/// How far beyond its ship's hull a drone comes out.
const LAUNCH_CLEARANCE: f64 = 150.0;
const THINK_MS: i64 = 1_000;

/// drone.py's activity states (eveConstants entity*).
pub mod activity {
    pub const IDLE: i64 = 0;
    pub const COMBAT: i64 = 1;
    pub const MINING: i64 = 2;
    pub const DEPARTING: i64 = 4;
    pub const DEPARTING_2: i64 = 5;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Order {
    Idle,
    Engage(i64),
    Mine { target: i64, repeat: bool },
    /// Back to the ship, to wait there.
    ReturnHome,
    /// Back into the drone bay.
    ReturnBay,
}

#[derive(Debug, Clone)]
pub struct Drone {
    pub owner: i64,
    pub client_id: i64,
    /// The ship it works for, while that ship is here.
    pub controller: Option<i64>,
    pub order: Order,
    pub activity: i64,
    weapons: Vec<Active>,
    miner: Option<Active>,
    /// Whether the miner has run since the last Mine order.
    mined: bool,
    orbit: f64,
    next_think_ms: i64,
}

/// Rows for SetState's droneState rowset:
/// droneID, ownerID, controllerID, activityState, typeID, controllerOwnerID.
pub fn state_lines(park: &Park) -> Vec<Value> {
    park.ships
        .values()
        .filter_map(|s| s.drone.as_ref().map(|d| (s, d)))
        .map(|(s, d)| Value::List(state_line(s.id, d, s.pilot.type_id)))
        .collect()
}

fn state_line(id: i64, d: &Drone, type_id: i64) -> Vec<Value> {
    vec![
        Value::Int(id),
        Value::Int(d.owner),
        Value::int_or_none(d.controller),
        Value::Int(d.activity),
        Value::Int(type_id),
        Value::Int(d.owner),
    ]
}

// ---- launching -----------------------------------------------------------------------------

/// ship.Drop(itemIDs): one drone into space per ID listed. Returns the
/// launched drones' IDs.
pub async fn launch(ctx: &Ctx<'_>, ids: Vec<i64>) -> Outcome {
    let (Some(char_id), Some(ship_id), Some(system)) = (ctx.session.char_id, ctx.session.ship_id, ctx.session.solar_system_id) else {
        return Outcome::custom("Drones can only be launched in space.");
    };
    let (ship_pos, ship_radius, out_already) = {
        let world = ctx.world.lock();
        let Some(park) = world.parks.get(&system) else { return Outcome::custom("You are not in space.") };
        let Some(ship) = park.ships.get(&ship_id) else { return Outcome::custom("You are not in space.") };
        let out = park.ships.values().filter(|s| s.drone.as_ref().is_some_and(|d| d.owner == char_id)).count();
        (ship.pos, ship.stats.radius, out)
    };
    let limit = match max_drones(ctx, char_id, ship_id).await {
        Ok(l) => l,
        Err(e) => {
            error!("launching drones: {}", crate::logging::chain(&e));
            return Outcome::custom("Your drones could not be launched.");
        }
    };
    if limit == 0 {
        return Outcome::custom("You need the Drones skill to launch drones.");
    }
    let room = limit.saturating_sub(out_already);
    if room == 0 {
        return Outcome::custom(format!("You can control at most {limit} drones."));
    }
    let wanted: Vec<i64> = ids.into_iter().take(room).collect();

    let mut rng = combat::Rng::seeded(combat::now_ms() as u64 ^ ship_id as u64);
    let result: Result<Option<(Vec<(i64, i64, Vec3)>, Vec<Value>)>, tokio_postgres::Error> = async {
        let mut client = crate::db::connect(ctx.db_url).await?;
        let mut txn = ItemTxn::begin(&mut client).await?;
        let mut launched = Vec::new();
        for id in wanted {
            let Some(item) = txn.item(id).await? else { continue };
            if item.owner_id != char_id || item.location_id != ship_id || item.flag != FLAG_DRONE_BAY || item.category_id != CATEGORY_DRONE {
                continue;
            }
            let drone = if item.quantity > 1 {
                txn.set_quantity(id, item.quantity - 1).await?;
                txn.create(NewItem { type_id: item.type_id, owner_id: char_id, location_id: system, flag: 0, quantity: 1, singleton: true })
                    .await?
            } else {
                txn.move_to(id, system, 0).await?;
                txn.set_singleton(id, true).await?;
                id
            };
            // Clear of the hull: destiny would shove a drone launched inside it.
            let pos = ship_pos + rng.in_sphere(1.0).unit() * (ship_radius + LAUNCH_CLEARANCE);
            txn.set_position(drone, pos.tuple()).await?;
            launched.push((drone, item.type_id, pos));
        }
        if launched.is_empty() {
            return Ok(None);
        }
        Ok(Some((launched, txn.commit().await?)))
    }
    .await;
    let (launched, events) = match result {
        Ok(Some(v)) => v,
        Ok(None) => return Outcome::custom("Those drones are not in your drone bay."),
        Err(e) => {
            error!("launching drones: {}", crate::logging::chain(&e));
            return Outcome::custom("Your drones could not be launched.");
        }
    };
    items::notify(ctx.world, ctx.client_id, events);

    let types: Vec<i64> = launched.iter().map(|(_, t, _)| *t).collect();
    let types = match npc::load_types(ctx.db, &types).await {
        Ok(t) => t,
        Err(e) => {
            error!("reading drone types: {}", crate::logging::chain(&e));
            HashMap::new()
        }
    };
    // The pilot's drone skills, through dogma.
    let drone_ids: Vec<i64> = launched.iter().map(|(id, _, _)| *id).collect();
    let skilled: HashMap<i64, HashMap<i32, f64>> =
        match crate::dogma::Loadout::load_with_drones(ctx.db, ctx.dogma, char_id, Some(ship_id), &Default::default(), &drone_ids).await {
            Ok(l) => drone_ids.iter().filter_map(|id| Some((*id, l.attributes(l.find(*id)?)))).collect(),
            Err(e) => {
                error!("applying drone skills: {}", crate::logging::chain(&e));
                HashMap::new()
            }
        };
    let now = combat::now_ms();
    let mut world = ctx.world.lock();
    let Some(park) = world.parks.get_mut(&system) else { return Outcome::Value(Value::List(Vec::new())) };
    let mut out = Out::default();
    let mut ids = Vec::new();
    for (id, type_id, pos) in launched {
        let Some(t) = types.get(&type_id) else { continue };
        let t = &t.with_attributes(skilled.get(&id));
        let pilot = Pilot {
            type_id,
            group_id: t.group_id,
            name: t.name.clone(),
            char_id,
            corp_id: ctx.session.corp_id.unwrap_or(0),
            alliance_id: ctx.session.alliance_id,
            security: 0.0,
            bounty: 0.0,
        };
        let drone = make(t, id, pilot, ctx.client_id, ship_id, pos, now, &mut park.rng);
        park.ships.insert(id, drone);
        for e in space::add_ball_entries(park, id) {
            out.destiny(system, e);
        }
        if let Some(d) = park.ships.get(&id).and_then(|s| s.drone.as_ref()) {
            out.note(ctx.client_id, "OnDroneStateChange", Value::Tuple(state_line(id, d, type_id)));
        }
        ids.push(id);
    }
    out.deliver(&world);
    info!("launched {} drone(s) from {ship_id}", ids.len());
    Outcome::Value(Value::List(ids.into_iter().map(Value::Int).collect()))
}

/// How many drones the pilot can run: the character's maxActiveDrones, which
/// the Drones skill raises.
async fn max_drones(ctx: &Ctx<'_>, char_id: i64, ship_id: i64) -> Result<usize, tokio_postgres::Error> {
    let loadout = crate::dogma::Loadout::load(ctx.db, ctx.dogma, char_id, Some(ship_id), &Default::default()).await?;
    Ok(loadout.character().map_or(0.0, |c| loadout.value(c, attr::MAX_ACTIVE_DRONES)).max(0.0) as usize)
}

#[allow(clippy::too_many_arguments)]
fn make(t: &npc::NpcType, id: i64, pilot: Pilot, client_id: i64, controller: i64, pos: Vec3, now: i64, rng: &mut combat::Rng) -> Ship {
    let a = |x: i32| t.get(x);
    let top = a(attr::MAX_VELOCITY).max(a(attr::ENTITY_CRUISE_SPEED)).max(100.0);
    let agility = if a(attr::AGILITY) > 0.0 { a(attr::AGILITY) } else { 1.0 };
    let mass = if t.mass > 0.0 { t.mass } else { 3_000.0 };
    let stats = ShipStats::new(t.radius.max(5.0), mass, agility, top, 1.0);
    let mut fighter = npc::entity_fighter(t, 60_000.0, 1, top, now);
    if fighter.vitals.hull_max <= 1.0 {
        fighter.vitals.hull_max = 50.0;
    }
    let miner = (a(attr::MINING_AMOUNT) > 0.0).then(|| {
        let action = Action::Mining { amount: a(attr::MINING_AMOUNT), range: a(attr::MAX_RANGE).max(1_000.0) };
        let mut m = npc::entity_module(t, id, EFFECT_MINING, "effects.Mining", action, a(attr::DURATION));
        m.offensive = false;
        m
    });
    let orbit = if a(attr::ORBIT_RANGE) > 0.0 { a(attr::ORBIT_RANGE) } else { a(attr::ENTITY_FLY_RANGE).max(500.0) };
    let drone = Drone {
        owner: pilot.char_id,
        client_id,
        controller: Some(controller),
        order: Order::Idle,
        activity: activity::IDLE,
        weapons: npc::weapons(t, &HashMap::new(), id, rng),
        miner,
        mined: false,
        orbit,
        next_think_ms: now,
    };
    let mut ship = Ship::new(id, client_id, stats, pilot, pos, Vec3::ZERO);
    ship.fight = Some(fighter);
    ship.drone = Some(drone);
    ship.command = Command::Orbit { target: controller, range: IDLE_ORBIT };
    ship
}

// ---- orders ----------------------------------------------------------------------------------

/// The solar system's `entity` object: drone orders. Each returns the drones
/// it could not order, as {droneID: (message, args)}.
pub async fn entity(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let ids: Vec<i64> = call.args.first().and_then(|v| v.seq()).unwrap_or(&[]).iter().filter_map(|v| v.as_i64()).collect();
    let target = call.arg_i64(1);
    let order = match call.method.as_str() {
        "CmdEngage" => Order::Engage(target?),
        "CmdMine" => Order::Mine { target: target?, repeat: false },
        "CmdMineRepeatedly" => Order::Mine { target: target?, repeat: true },
        "CmdReturnHome" => Order::ReturnHome,
        "CmdReturnBay" => Order::ReturnBay,
        "CmdAbandonDrone" | "CmdRelinquishControl" => Order::Idle,
        "CmdDelegateControl" | "CmdUnanchor" => {
            return Some(Outcome::custom("Your drones cannot do that."));
        }
        _ => return None,
    };
    Some(Outcome::Value(command(ctx, &ids, order)))
}

fn command(ctx: &Ctx<'_>, ids: &[i64], order: Order) -> Value {
    let (Some(char_id), Some(system)) = (ctx.session.char_id, ctx.session.solar_system_id) else {
        return Value::Dict(Vec::new());
    };
    let mut failed = Vec::new();
    let mut world = ctx.world.lock();
    let Some(park) = world.parks.get_mut(&system) else { return Value::Dict(Vec::new()) };
    let target_ok = match &order {
        Order::Engage(t) => park.ships.get(t).is_some_and(|s| s.fight.is_some() && s.pilot.char_id != char_id),
        Order::Mine { target, .. } => park.rocks.contains_key(target),
        _ => true,
    };
    for id in ids {
        let Some(drone) = park.ships.get_mut(id).and_then(|s| s.drone.as_mut()).filter(|d| d.owner == char_id) else {
            failed.push((*id, "That is not your drone."));
            continue;
        };
        if !target_ok {
            failed.push((*id, "Your drones cannot do that to that target."));
            continue;
        }
        if matches!(order, Order::Mine { .. }) && drone.miner.is_none() {
            failed.push((*id, "That drone cannot mine."));
            continue;
        }
        drone.order = order.clone();
        drone.mined = false;
        drone.next_think_ms = 0;
        if drone.controller.is_none() {
            drone.controller = ctx.session.ship_id;
        }
    }
    debug!("drones {ids:?}: {order:?}");
    Value::Dict(
        failed
            .into_iter()
            .map(|(id, why)| {
                let args = Value::Dict(vec![(Value::bytes("error"), Value::text(why))]);
                (Value::Int(id), Value::Tuple(vec![Value::bytes("CustomError"), args]))
            })
            .collect(),
    )
}

// ---- thinking ---------------------------------------------------------------------------------

pub fn think(park: &mut Park, now: i64, out: &mut Out) {
    let due: Vec<i64> = park
        .ships
        .values()
        .filter(|s| s.drone.as_ref().is_some_and(|d| d.next_think_ms <= now))
        .map(|s| s.id)
        .collect();
    for id in due {
        think_one(park, id, now, out);
    }
}

fn think_one(park: &mut Park, id: i64, now: i64, out: &mut Out) {
    let Some(drone) = park.ships.get(&id).and_then(|s| s.drone.clone()) else { return };
    let controller = drone.controller.filter(|c| park.ships.contains_key(c));
    let mut order = drone.order.clone();
    // An order with nothing left to do becomes waiting.
    let gone = match &order {
        Order::Engage(t) => !park.ships.contains_key(t),
        Order::Mine { target, .. } => !park.rocks.contains_key(target),
        Order::ReturnHome | Order::ReturnBay => controller.is_none(),
        Order::Idle => false,
    };
    if gone {
        order = Order::Idle;
    }
    let running = |park: &Park, module: i64| {
        park.ships.get(&id).and_then(|s| s.fight.as_ref()).is_some_and(|f| f.module(module).is_some())
    };
    if let Order::Mine { repeat: false, .. } = order {
        let miner = drone.miner.as_ref().map(|m| m.module_id);
        if drone.mined && miner.is_some_and(|m| !running(park, m)) {
            order = Order::ReturnBay;
        }
    }

    let (activity, course, work): (i64, Option<Command>, Option<i64>) = match &order {
        Order::Idle => (activity::IDLE, controller.map(|c| Command::Orbit { target: c, range: IDLE_ORBIT }), None),
        Order::Engage(t) => (activity::COMBAT, Some(Command::Orbit { target: *t, range: drone.orbit }), Some(*t)),
        Order::Mine { target, .. } => (activity::MINING, Some(Command::Orbit { target: *target, range: drone.orbit.min(500.0) }), Some(*target)),
        Order::ReturnHome => (activity::DEPARTING_2, controller.map(|c| Command::Follow { target: c, range: IDLE_ORBIT }), None),
        Order::ReturnBay => (activity::DEPARTING, controller.map(|c| Command::Follow { target: c, range: 0.0 }), None),
    };

    // Home: into the bay.
    if order == Order::ReturnBay {
        if let Some(c) = controller {
            if park.surface_distance(id, c).is_some_and(|d| d <= DOCK_RANGE) {
                dock(park, id, c, now, out);
                return;
            }
        }
    }
    if order == Order::ReturnHome {
        if let Some(c) = controller {
            if park.surface_distance(id, c).is_some_and(|d| d <= IDLE_ORBIT * 1.5) {
                order = Order::Idle;
            }
        }
    }

    // Stop what it should no longer do.
    let keep: Vec<i64> = match work {
        Some(_) => drone.weapons.iter().map(|w| w.module_id).chain(drone.miner.iter().map(|m| m.module_id)).collect(),
        None => Vec::new(),
    };
    let stale: Vec<i64> = park
        .ships
        .get(&id)
        .and_then(|s| s.fight.as_ref())
        .map(|f| f.modules.iter().filter(|m| !keep.contains(&m.module_id) || m.target != work).map(|m| m.module_id).collect())
        .unwrap_or_default();
    for m in stale {
        crate::fight::stop(park, id, m, now, out, None);
    }

    let tick = park.tick;
    let system = park.solar_system_id;
    let signature = work.and_then(|t| park.ships.get(&t)).and_then(|s| s.fight.as_ref()).map_or(100.0, |f| f.sig());
    let Some(ship) = park.ships.get_mut(&id) else { return };
    let Some(fight) = ship.fight.as_mut() else { return };
    match work {
        Some(t) => {
            fight.locks.retain(|l, _| *l == t);
            if !fight.locks.contains_key(&t) {
                let ms = (combat::lock_seconds(fight.scan_res(), signature) * 1_000.0) as i64;
                fight.locks.insert(t, Lock::Pending(now + ms));
            }
        }
        None => fight.locks.clear(),
    }
    let mut started = Vec::new();
    if let Some(t) = work.filter(|t| fight.is_locked(*t)) {
        let tools: Vec<Active> = match order {
            Order::Mine { repeat, .. } if !drone.mined => {
                drone.miner.iter().cloned().map(|mut m| {
                    m.repeat = if repeat { REPEAT_FOREVER } else { 0 };
                    m
                }).collect()
            }
            Order::Engage(_) => drone.weapons.clone(),
            _ => Vec::new(),
        };
        for mut m in tools {
            if fight.module(m.module_id).is_some() {
                continue;
            }
            m.target = Some(t);
            m.started_ms = now;
            m.next_ms = now;
            started.push(m.clone());
            fight.modules.push(m);
        }
    }
    let Some(d) = ship.drone.as_mut() else { return };
    if !started.is_empty() && matches!(order, Order::Mine { .. }) {
        d.mined = true;
    }
    let activity = if order == Order::Idle { activity::IDLE } else { activity };
    let changed = d.activity != activity;
    d.order = order;
    d.activity = activity;
    d.controller = controller.or(d.controller);
    d.next_think_ms = now + THINK_MS;
    let line = state_line(id, d, ship.pilot.type_id);
    let client = d.client_id;
    if changed {
        out.note(client, "OnDroneStateChange", Value::Tuple(line));
    }
    for m in &started {
        out.destiny(system, crate::fight::module_fx(tick, id, m, true));
    }
    let course = course.unwrap_or(Command::Stop);
    if ship.command != course {
        park.command(id, course);
        park.set_speed_fraction(id, 1.0);
        if let Some(s) = park.ships.get(&id) {
            for e in space::course_entries(s, tick) {
                out.destiny(system, e);
            }
            if s.command == Command::Stop {
                out.destiny(system, space::command_entry(s, tick));
            }
        }
    }
}

/// Into the bay: out of space, and the item back where it came from.
fn dock(park: &mut Park, id: i64, controller: i64, now: i64, out: &mut Out) {
    let Some(ship) = crate::fight::detach(park, id, now, out) else { return };
    out.destiny(park.solar_system_id, space::entry(park.tick, "RemoveBalls", vec![Value::List(vec![Value::Int(id)])]));
    let client_id = ship.drone.as_ref().map_or(ship.client_id, |d| d.client_id);
    out.jobs.push(Job::DroneHome { client_id, drone_id: id, ship_id: controller });
}

/// ship.ScoopDrone(ids): drones close to the ship go straight into the bay.
pub async fn scoop(ctx: &Ctx<'_>, ids: Vec<i64>) -> Outcome {
    let (Some(char_id), Some(ship_id), Some(system)) = (ctx.session.char_id, ctx.session.ship_id, ctx.session.solar_system_id) else {
        return Outcome::custom("You are not in space.");
    };
    let mut scooped = Vec::new();
    {
        let mut world = ctx.world.lock();
        let Some(park) = world.parks.get_mut(&system) else { return Outcome::custom("You are not in space.") };
        let mut out = Out::default();
        for id in ids {
            let mine = park.ships.get(&id).and_then(|s| s.drone.as_ref()).is_some_and(|d| d.owner == char_id);
            let near = park.surface_distance(id, ship_id).is_some_and(|d| d <= SCOOP_RANGE * 1.25);
            if mine && near && crate::fight::detach(park, id, combat::now_ms(), &mut out).is_some() {
                out.destiny(system, space::entry(park.tick, "RemoveBalls", vec![Value::List(vec![Value::Int(id)])]));
                scooped.push(id);
            }
        }
        out.deliver(&world);
    }
    if scooped.is_empty() {
        return Outcome::custom("There is no drone of yours close enough to scoop.");
    }
    let mut client = match crate::db::connect(ctx.db_url).await {
        Ok(c) => c,
        Err(e) => {
            error!("scooping drones: {}", crate::logging::chain(&e));
            return Outcome::custom("Your drones could not be scooped.");
        }
    };
    for id in scooped {
        into_bay(&mut client, ctx.world, ctx.client_id, id, ship_id).await;
    }
    Outcome::Value(Value::None)
}

pub async fn home(server: &Server, db: &mut Client, client_id: i64, drone_id: i64, ship_id: i64) {
    into_bay(db, &server.world, client_id, drone_id, ship_id).await;
}

async fn into_bay(db: &mut Client, world: &World, client_id: i64, drone_id: i64, ship_id: i64) {
    let result = async {
        let mut txn = ItemTxn::begin(db).await?;
        txn.move_to(drone_id, ship_id, FLAG_DRONE_BAY).await?;
        txn.commit().await
    }
    .await;
    match result {
        Ok(events) => {
            items::notify(world, client_id, events);
            debug!("drone {drone_id} back in {ship_id}");
        }
        Err(e) => error!("returning drone {drone_id} to {ship_id}: {}", crate::logging::chain(&e)),
    }
}

pub async fn lost(server: &Server, db: &mut Client, client_id: Option<i64>, drone_id: i64) {
    let result = async {
        let mut txn = ItemTxn::begin(db).await?;
        txn.destroy(drone_id).await?;
        txn.commit().await
    }
    .await;
    match result {
        Ok(events) => {
            if let Some(client) = client_id {
                items::notify(&server.world, client, events);
            }
        }
        Err(e) => warn!("destroying drone {drone_id}: {}", crate::logging::chain(&e)),
    }
}
