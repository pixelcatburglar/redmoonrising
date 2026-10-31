//! `beyonce`, the remote ballpark: what the client sees in space and how its
//! ship moves (michelle.py, docs: destiny/ballpark/michelle).
//!
//! Every solar system with a ship in it has a [`Park`] in the shared world. A
//! movement command updates the park and goes out as a destiny opcode to every
//! client in the system, which then runs the same motion locally. A client that
//! (re)builds its ballpark gets a SetState of the whole park.

use eve_marshal::Value;
use tracing::{debug, warn};

use crate::ballpark::{Celestial, Command, Park, Ship, Vec3};
use crate::destiny::{self, flags, mode, Ball};
use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{float, int, key_val, rowset, text};
use crate::world::WorldState;

const TYPE_SOLAR_SYSTEM: i64 = 5;
const OWNER_SYSTEM: i64 = 1;
const GROUP_STARGATE: i64 = 10;
const GROUP_STATION: i64 = 15;
/// Sun, planet, moon, asteroid belt, stargate, station.
const CELESTIAL_GROUPS: [i32; 6] = [6, 7, 8, 9, 10, 15];
/// WarpToStuff without minRange (menusvc's default warp-to distance).
const DEFAULT_WARP_RANGE: f64 = 15_000.0;
/// The menu's "approach" with no range (menusvc.py:1598 `range or 50`).
const DEFAULT_FOLLOW_RANGE: f64 = 50.0;

/// `ship`: Undock is served by `conn` (it changes the session); nothing else
/// on the ship object is implemented yet.
/// The `ship` service. Undock and Board change the session, so the connection
/// handles them.
pub async fn ship(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    match call.method.as_str() {
        "AssembleShip" => {
            let ids = call.args.first().and_then(|v| v.seq()).unwrap_or(&[]);
            let ids = ids.iter().filter_map(|v| v.as_i64()).collect();
            Some(crate::svc::moves::assemble_ship(ctx, ids).await)
        }
        // Drop(itemIDs, ownerID=None, ignoreWarning=False): launch structures or
        // drones, one per ID listed (a stack of five is listed five times).
        "Drop" => {
            let ids = call.args.first().and_then(|v| v.seq()).unwrap_or(&[]);
            let ids: Vec<i64> = ids.iter().filter_map(|v| v.as_i64()).collect();
            let owner = call.args.get(1).and_then(|v| v.as_i64());
            if let Some(outcome) = crate::svc::starbase::launch(ctx, &ids, owner).await {
                return Some(outcome);
            }
            Some(crate::drones::launch(ctx, ids).await)
        }
        // Scoop(itemID, password=None): a structure floating free.
        "Scoop" => crate::svc::starbase::scoop(ctx, call.arg_id(0)).await,
        "ScoopDrone" => {
            let ids = call.args.first().and_then(|v| v.seq()).unwrap_or(&[]);
            let ids: Vec<i64> = ids.iter().filter_map(|v| v.as_i64()).collect();
            Some(crate::drones::scoop(ctx, ids).await)
        }
        _ => None,
    }
}

pub async fn beyonce(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        // LoadFormations accepts a tuple; no formations exist.
        "GetFormations" => {
            push_set_state(ctx).await;
            Value::Tuple(Vec::new())
        }
        // michelle.RequestReset: the client lost sync and wants the world again.
        "UpdateStateRequest" => {
            push_set_state(ctx).await;
            Value::None
        }
        "GetGangState" => Value::None,
        "GetRelativity" => relativity(ctx, call),
        method if is_movement(method) => return Some(movement(ctx, call).await),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

pub fn is_movement(method: &str) -> bool {
    matches!(
        method,
        "SetSpeedFraction" | "GotoDirection" | "GotoPoint" | "Stop" | "FollowBall" | "Orbit" | "WarpToStuff"
    )
}

// ---- the park ---------------------------------------------------------------

/// Makes sure `solar_system_id` has a park, loading its celestials if not.
pub async fn ensure_park(ctx: &Ctx<'_>, solar_system_id: i64) {
    if ctx.world.lock().parks.contains_key(&solar_system_id) {
        return;
    }
    let celestials = load_celestials(ctx, solar_system_id).await;
    let now = crate::combat::now_ms();
    if let Err(e) = crate::loot::ensure_table(ctx.db).await {
        warn!("no loot table: {}", crate::logging::chain(&e));
    }
    let cans = crate::loot::load_cans(ctx.db, solar_system_id, now).await.unwrap_or_else(|e| {
        warn!("cans in {solar_system_id} not loaded: {}", crate::logging::chain(&e));
        Vec::new()
    });
    let structures = crate::pos::load(ctx.db, solar_system_id).await.unwrap_or_else(|e| {
        warn!("structures in {solar_system_id} not loaded: {}", crate::logging::chain(&e));
        Vec::new()
    });
    let wildlife = match crate::npc::load(ctx.db, solar_system_id).await {
        Ok(w) => w,
        Err(e) => {
            warn!("system {solar_system_id} gets no belt life: {}", crate::logging::chain(&e));
            Default::default()
        }
    };
    let mut world = ctx.world.lock();
    world.parks.entry(solar_system_id).or_insert_with(|| {
        let mut park = Park::new(solar_system_id, celestials, destiny::tick_now());
        park.wildlife = std::sync::Arc::new(wildlife);
        crate::npc::populate(&mut park);
        park.cans = cans.into_iter().map(|c| (c.id, c)).collect();
        park.structures = structures.into_iter().map(|s| (s.id, s)).collect();
        park
    });
}

async fn load_celestials(ctx: &Ctx<'_>, solar_system_id: i64) -> Vec<Celestial> {
    let sql = r#"SELECT d."itemID", d."typeID", d."groupID", COALESCE(d."itemName", ''),
                        d."x", d."y", d."z", COALESCE(d."radius", 0),
                        j."celestialID", dest."solarSystemID"
                   FROM "mapDenormalize" d
                   LEFT JOIN "mapJumps" j ON j."stargateID" = d."itemID"
                   LEFT JOIN "mapDenormalize" dest ON dest."itemID" = j."celestialID"
                  WHERE d."solarSystemID" = $1 AND d."groupID" = ANY($2)
                  ORDER BY d."itemID""#;
    let rows = ctx.query(sql, &[&(solar_system_id as i32), &&CELESTIAL_GROUPS[..]]).await;
    rows.iter()
        .map(|r| Celestial {
            id: int(r, 0).unwrap_or(0),
            type_id: int(r, 1).unwrap_or(0),
            group_id: int(r, 2).unwrap_or(0),
            name: text(r, 3).unwrap_or_default(),
            pos: Vec3::new(float(r, 4).unwrap_or(0.0), float(r, 5).unwrap_or(0.0), float(r, 6).unwrap_or(0.0)),
            radius: float(r, 7).unwrap_or(0.0),
            jump: match (int(r, 9), int(r, 8)) {
                (Some(system), Some(gate)) => Some((system, gate)),
                _ => None,
            },
        })
        .collect()
}

// ---- what the client is sent --------------------------------------------------

pub fn entry(tick: i64, opcode: &str, args: Vec<Value>) -> Value {
    Value::Tuple(vec![
        Value::Int(destiny::stamp(tick) as i64),
        Value::Tuple(vec![Value::bytes(opcode), Value::Tuple(args)]),
    ])
}

/// DoDestinyUpdate(state, waitForBubble=0) to every client in the system, or
/// only to `only`.
pub fn broadcast(world: &WorldState, solar_system_id: i64, only: Option<i64>, entries: Vec<Value>) {
    if entries.is_empty() {
        return;
    }
    let targets = match only {
        Some(c) => vec![c],
        None => world.in_space(solar_system_id),
    };
    let args = Value::Tuple(vec![Value::List(entries), Value::Int(0)]);
    world.notify(&targets, "DoDestinyUpdate", args);
}

fn ship_ball(ship: &Ship) -> Ball {
    Ball {
        id: ship.id,
        mode: mode::STOP,
        radius: ship.stats.radius,
        pos: ship.pos.tuple(),
        flags: if ship.in_warp() { flags::SHIP_IN_WARP } else { flags::SHIP },
        mass: ship.stats.mass,
        corp_id: ship.pilot.corp_id as i32,
        alliance_id: ship.pilot.alliance_id.unwrap_or(-1) as i32,
        harmonic: ship.harmonic,
        max_velocity: ship.stats.max_velocity,
        velocity: ship.vel.tuple(),
        agility: ship.stats.agility,
        speed_fraction: ship.speed_fraction,
        name: ship.pilot.name.clone(),
    }
}

pub fn slim(fields: Vec<(&str, Value)>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("foo.SlimItem")),
        state: Box::new(Value::Dict(fields.into_iter().map(|(k, v)| (Value::bytes(k), v)).collect())),
    }
}

/// groupID/categoryID are left out on purpose: the client derives them from typeID.
fn ship_slim(ship: &Ship) -> Value {
    let p = &ship.pilot;
    // An NPC belongs to its corporation and has no pilot to show.
    if ship.npc.is_some() {
        return slim(vec![
            ("itemID", Value::Int(ship.id)),
            ("typeID", Value::Int(p.type_id)),
            ("ownerID", Value::Int(p.corp_id)),
            ("corpID", Value::Int(p.corp_id)),
        ]);
    }
    // A drone belongs to its pilot but is not flown by one.
    if ship.drone.is_some() {
        return slim(vec![
            ("itemID", Value::Int(ship.id)),
            ("typeID", Value::Int(p.type_id)),
            ("ownerID", Value::Int(p.char_id)),
            ("corpID", Value::Int(p.corp_id)),
        ]);
    }
    slim(vec![
        ("itemID", Value::Int(ship.id)),
        ("typeID", Value::Int(p.type_id)),
        ("ownerID", Value::Int(p.char_id)),
        ("charID", Value::Int(p.char_id)),
        ("corpID", Value::Int(p.corp_id)),
        ("allianceID", Value::int_or_none(p.alliance_id)),
        // consider.py rounds it; it must be a number.
        ("securityStatus", Value::Double(p.security)),
        ("bounty", Value::Double(p.bounty)),
    ])
}

fn celestial_slim(c: &Celestial, owners: &[(i64, i64)]) -> Value {
    let owner = owners.iter().find(|(id, _)| *id == c.id).map_or(OWNER_SYSTEM, |(_, o)| *o);
    let mut fields = vec![
        ("itemID", Value::Int(c.id)),
        ("typeID", Value::Int(c.type_id)),
        ("ownerID", Value::Int(owner)),
    ];
    if let Some((system, gate)) = c.jump {
        let jump = crate::svc::rows::row(
            &["locationID", "toCelestialID"],
            vec![Value::Int(system), Value::Int(gate)],
        );
        fields.push(("jumps", Value::List(vec![jump])));
    }
    slim(fields)
}

/// [shield (fraction, tau), armor, structure], as the ship is now.
fn health(ship: &Ship) -> Value {
    match &ship.fight {
        Some(f) => crate::fight::damage_state(&f.vitals, crate::combat::now_ms()),
        None => Value::List(vec![
            Value::Tuple(vec![Value::Double(1.0), Value::Double(1.0)]),
            Value::Double(1.0),
            Value::Double(1.0),
        ]),
    }
}

/// The destiny opcode for a ship's current command.
pub fn command_entry(ship: &Ship, tick: i64) -> Value {
    let id = Value::Int(ship.id);
    let point = |p: &Vec3| vec![id.clone(), Value::Double(p.x), Value::Double(p.y), Value::Double(p.z)];
    match &ship.command {
        Command::Stop => entry(tick, "Stop", vec![id]),
        Command::Direction(d) => entry(tick, "GotoDirection", point(d)),
        Command::Point(p) => entry(tick, "GotoPoint", point(p)),
        Command::Follow { target, range } => {
            entry(tick, "FollowBall", vec![id, Value::Int(*target), Value::Double(*range)])
        }
        Command::Orbit { target, range } => {
            entry(tick, "Orbit", vec![id, Value::Int(*target), Value::Double(*range)])
        }
        // WarpTo(ballID, x, y, z, range, warpFactor): without the factor
        // destiny warps everyone at 2 AU/s.
        Command::Warp { target, range } => {
            let mut args = point(target);
            args.push(Value::Double(*range));
            args.push(Value::Int(ship.stats.warp_factor));
            entry(tick, "WarpTo", args)
        }
    }
}

/// The opcodes that put a ship back on its current course, for a client that
/// has just been given the ship standing still.
pub fn course_entries(ship: &Ship, tick: i64) -> Vec<Value> {
    if ship.command == Command::Stop {
        return Vec::new();
    }
    let speed = entry(tick, "SetSpeedFraction", vec![Value::Int(ship.id), Value::Double(ship.speed_fraction)]);
    vec![command_entry(ship, tick), speed]
}

/// SetState for `ego`: the whole park, then every moving ship's course.
pub fn set_state_entries(park: &Park, ego: i64, station_owners: &[(i64, i64)], char_gangs: &[(i64, i64)]) -> Vec<Value> {
    const DRONE_HEADER: &[&str] =
        &["droneID", "ownerID", "controllerID", "activityState", "typeID", "controllerOwnerID"];
    let mut balls: Vec<Ball> = park
        .celestials
        .iter()
        .map(|c| Ball::celestial(c.id, c.radius, c.pos.tuple(), &c.name, c.massive()))
        .collect();
    let mut slims: Vec<Value> = park.celestials.iter().map(|c| celestial_slim(c, station_owners)).collect();
    for rock in park.rocks.values() {
        balls.push(Ball::celestial(rock.id, rock.radius, rock.pos.tuple(), &rock.name, true));
        slims.push(crate::npc::rock_slim(rock));
    }
    for can in park.cans.values() {
        balls.push(Ball::celestial(can.id, can.radius, can.pos.tuple(), &can.name, false));
        slims.push(crate::loot::slim(can));
    }
    for site in park.sites.values() {
        balls.push(site.ball());
        slims.push(site.slim());
    }
    let mut damage = Vec::new();
    let now_ms = crate::combat::now_ms();
    for s in park.structures.values() {
        balls.push(s.ball());
        slims.push(s.slim());
        damage.push((Value::Int(s.id), crate::fight::damage_state(&s.vitals, now_ms)));
        if let Some(f) = &s.field {
            balls.push(f.ball(s));
            slims.push(f.slim(s));
        }
    }
    for ship in park.ships.values() {
        balls.push(ship_ball(ship));
        slims.push(ship_slim(ship));
        damage.push((Value::Int(ship.id), health(ship)));
    }
    let bag = key_val(vec![
        ("state", Value::Bytes(destiny::encode(destiny::stamp(park.tick), &balls))),
        ("ego", Value::Int(ego)),
        ("slims", Value::List(slims)),
        (
            "solItem",
            slim(vec![
                ("itemID", Value::Int(park.solar_system_id)),
                ("typeID", Value::Int(TYPE_SOLAR_SYSTEM)),
                ("ownerID", Value::Int(OWNER_SYSTEM)),
            ]),
        ),
        ("aggressors", park.watch.aggressors()),
        ("droneState", rowset(DRONE_HEADER, crate::drones::state_lines(park))),
        ("gangCorps", Value::List(Vec::new())),
        (
            "charGangs",
            Value::List(char_gangs.iter().map(|(c, g)| Value::Tuple(vec![Value::Int(*c), Value::Int(*g)])).collect()),
        ),
        ("damageState", Value::Dict(damage)),
        ("effectStates", Value::List(Vec::new())),
    ]);
    let mut entries = vec![entry(park.tick, "SetState", vec![bag])];
    for ship in park.ships.values() {
        entries.extend(course_entries(ship, park.tick));
    }
    entries
}

/// AddBalls for one ship joining the park, then its course.
pub fn add_ball_entries(park: &Park, ship_id: i64) -> Vec<Value> {
    let Some(ship) = park.ships.get(&ship_id) else { return Vec::new() };
    let blob = destiny::encode(destiny::stamp(park.tick), &[ship_ball(ship)]);
    let chunk = Value::Tuple(vec![
        Value::Bytes(blob),
        Value::List(vec![ship_slim(ship)]),
        Value::Dict(vec![(Value::Int(ship.id), health(ship))]),
    ]);
    let mut entries = vec![entry(park.tick, "AddBalls", vec![chunk])];
    entries.extend(course_entries(ship, park.tick));
    entries
}

pub fn remove_ball_entries(tick: i64, ship_id: i64) -> Vec<Value> {
    vec![entry(tick, "RemoveBalls", vec![Value::List(vec![Value::Int(ship_id)])])]
}

/// OnSpecialFX(shipID, moduleID, moduleTypeID, targetID, otherTypeID, area, guid,
/// isOffensive, start, active) as a destiny entry (spaceMgr.py:120).
pub fn special_fx(tick: i64, ship_id: i64, target_id: Option<i64>, guid: &str) -> Value {
    entry(
        tick,
        "OnSpecialFX",
        vec![
            Value::Int(ship_id),
            Value::None,
            Value::None,
            Value::int_or_none(target_id),
            Value::None,
            Value::List(Vec::new()),
            Value::bytes(guid),
            Value::Int(0),
            Value::Int(1),
            Value::Int(0),
        ],
    )
}

/// Stations' owning corporations, for their slims.
pub async fn station_owners(ctx: &Ctx<'_>, solar_system_id: i64) -> Vec<(i64, i64)> {
    let sql = r#"SELECT "stationID", "corporationID" FROM "staStations" WHERE "solarSystemID" = $1"#;
    ctx.query(sql, &[&(solar_system_id as i32)])
        .await
        .iter()
        .filter_map(|r| Some((int(r, 0)?, int(r, 1)?)))
        .collect()
}

/// Puts a ship into its system's park and shows it to everyone else there.
pub fn add_ship(world: &mut WorldState, system: i64, ship: Ship) {
    let ship_id = ship.id;
    let owner = ship.client_id;
    let Some(park) = world.parks.get_mut(&system) else { return };
    park.advance(destiny::tick_now());
    park.ships.insert(ship_id, ship);
    let entries = add_ball_entries(park, ship_id);
    let others: Vec<i64> = world.in_space(system).into_iter().filter(|c| *c != owner).collect();
    let args = Value::Tuple(vec![Value::List(entries), Value::Int(0)]);
    world.notify(&others, "DoDestinyUpdate", args);
}

/// Gives a ship in a park a new course and tells the system (a warp opens
/// the throttle first, as destiny wants).
pub fn order(world: &mut WorldState, system: i64, ship_id: i64, command: Command) {
    let Some(park) = world.parks.get_mut(&system) else { return };
    park.advance(destiny::tick_now());
    if !park.ships.contains_key(&ship_id) {
        return;
    }
    let tick = park.tick;
    let mut entries = Vec::new();
    if matches!(command, Command::Warp { .. }) && park.ships[&ship_id].speed_fraction < 1.0 {
        park.set_speed_fraction(ship_id, 1.0);
        entries.push(entry(tick, "SetSpeedFraction", vec![Value::Int(ship_id), Value::Double(1.0)]));
    }
    park.command(ship_id, command);
    entries.insert(0, command_entry(&park.ships[&ship_id], tick));
    broadcast(world, system, None, entries);
}

/// Takes a ship out of its system's park, out of everyone's view and out of
/// every fight it was in; returns it.
pub fn remove_ship(world: &mut WorldState, system: i64, ship_id: i64) -> Option<Ship> {
    let park = world.parks.get_mut(&system)?;
    park.advance(destiny::tick_now());
    let mut out = crate::heartbeat::Out::default();
    let now = crate::combat::now_ms();
    crate::fight::stand_down(park, ship_id, now, &mut out);
    let ship = crate::fight::detach(park, ship_id, now, &mut out)?;
    let entries = remove_ball_entries(park.tick, ship_id);
    out.deliver(world);
    let others: Vec<i64> = world.in_space(system).into_iter().filter(|c| *c != ship.client_id).collect();
    let args = Value::Tuple(vec![Value::List(entries), Value::Int(0)]);
    world.notify(&others, "DoDestinyUpdate", args);
    Some(ship)
}

/// Queues a SetState for the caller's own ship.
async fn push_set_state(ctx: &Ctx<'_>) {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return;
    };
    ensure_park(ctx, system).await;
    let owners = station_owners(ctx, system).await;
    let mut world = ctx.world.lock();
    let Some(park) = world.parks.get_mut(&system) else { return };
    park.advance(destiny::tick_now());
    if !park.ships.contains_key(&ship_id) {
        warn!("SetState for ship {ship_id}, which is not in the park of {system}");
    }
    // Just out of a gate: the pilot sees the arrival too, once their park is up.
    let arrival = park.ships.get_mut(&ship_id).and_then(|s| s.arrived_through.take());
    let pilots: Vec<i64> = park.ships.values().map(|s| s.pilot.char_id).filter(|c| *c != 0).collect();
    let char_gangs = world.gangs.memberships(pilots);
    let Some(park) = world.parks.get(&system) else { return };
    let mut entries = set_state_entries(park, ship_id, &owners, &char_gangs);
    if let Some(gate) = arrival {
        entries.push(special_fx(park.tick, ship_id, Some(gate), "effects.JumpIn"));
    }
    debug!("SetState: {} ball(s), ego {ship_id}", park.celestials.len() + park.ships.len());
    broadcast(&world, system, Some(ctx.client_id), entries);
    world.sees(ctx.client_id, system);
}

/// (diff, x, y, z, serverTick, tTime, rTime): where the server has the ship, and
/// how far that is from where the client has it (michelle.GetRelativity).
fn relativity(ctx: &Ctx<'_>, call: &Call) -> Value {
    let client_pos = Vec3::new(
        call.args.first().and_then(|v| v.as_f64()).unwrap_or(0.0),
        call.args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0),
        call.args.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0),
    );
    let tick = destiny::tick_now();
    let server_pos = ctx
        .session
        .solar_system_id
        .zip(ctx.session.ship_id)
        .and_then(|(system, ship)| {
            let mut world = ctx.world.lock();
            let park = world.parks.get_mut(&system)?;
            park.advance(tick);
            park.ships.get(&ship).map(|s| s.pos)
        })
        .unwrap_or(client_pos);
    Value::Tuple(vec![
        Value::Double((server_pos - client_pos).len()),
        Value::Double(server_pos.x),
        Value::Double(server_pos.y),
        Value::Double(server_pos.z),
        Value::Int(destiny::stamp(tick) as i64),
        Value::Long(crate::bulkdata::filetime_now()),
        Value::Long(crate::bulkdata::filetime_now()),
    ])
}

// ---- movement -------------------------------------------------------------------

enum Order {
    Command(Command),
    SpeedFraction(f64),
}

/// A movement call on the caller's ship: update the park, tell the system.
async fn movement(ctx: &Ctx<'_>, call: &Call) -> Outcome {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return Outcome::UserError { msg: "ShipNotYours".into(), args: Vec::new() };
    };
    let Some(order) = parse_order(ctx, call).await else {
        return Outcome::Unknown(format!("beyonce::{} with arguments it cannot use", call.method));
    };
    steer(ctx, system, ship_id, order, &call.method)
}

/// Warps the caller's ship to `range` off something at `at` of `radius`
/// (an agent's location, say).
pub fn warp_to_point(ctx: &Ctx<'_>, at: Vec3, radius: f64, range: f64, why: &str) -> Outcome {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return Outcome::UserError { msg: "ShipNotYours".into(), args: Vec::new() };
    };
    let Some(target) = landing_point(ctx, at, radius + range.max(0.0)) else {
        return Outcome::custom("Your ship is not in space here.");
    };
    steer(ctx, system, ship_id, Order::Command(Command::Warp { target, range: 0.0 }), why)
}

/// Flies the caller's ship straight to `at`.
pub fn approach_point(ctx: &Ctx<'_>, at: Vec3, why: &str) -> Outcome {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return Outcome::UserError { msg: "ShipNotYours".into(), args: Vec::new() };
    };
    steer(ctx, system, ship_id, Order::Command(Command::Point(at)), why)
}

/// Puts a movement order to a ship: the park follows it, and the system is told.
fn steer(ctx: &Ctx<'_>, system: i64, ship_id: i64, order: Order, method: &str) -> Outcome {
    let mut world = ctx.world.lock();
    let Some(park) = world.parks.get_mut(&system) else {
        return Outcome::Unknown(format!("beyonce::{method}: no ballpark for {system}"));
    };
    park.advance(destiny::tick_now());
    let tick = park.tick;
    match park.ships.get(&ship_id) {
        None => return Outcome::Unknown(format!("beyonce::{method}: ship {ship_id} is not in space")),
        // destiny ignores course changes mid-warp; so does the server.
        Some(ship) if ship.in_warp() => return Outcome::Value(Value::None),
        Some(ship)
            if matches!(order, Order::Command(Command::Warp { .. }))
                && ship.fight.as_ref().is_some_and(|f| f.afflictions.scrambled >= 1.0) =>
        {
            return Outcome::UserError { msg: "WarpScrambled".into(), args: Vec::new() };
        }
        Some(_) => {}
    }
    let (entries, what) = match order {
        Order::SpeedFraction(f) => {
            park.set_speed_fraction(ship_id, f);
            let speed = entry(tick, "SetSpeedFraction", vec![Value::Int(ship_id), Value::Double(f)]);
            (vec![speed], format!("speed {f:.2}"))
        }
        Order::Command(command) => {
            let what = format!("{command:?}");
            let mut entries = Vec::new();
            // destiny aligns at the throttle's speed and only warps above
            // 75% of the top one, so a warp opens it.
            if matches!(command, Command::Warp { .. }) && park.ships[&ship_id].speed_fraction < 1.0 {
                park.set_speed_fraction(ship_id, 1.0);
                entries.push(entry(tick, "SetSpeedFraction", vec![Value::Int(ship_id), Value::Double(1.0)]));
            }
            park.command(ship_id, command);
            entries.insert(0, command_entry(&park.ships[&ship_id], tick));
            (entries, what)
        }
    };
    debug!("ship {ship_id}: {what}");
    broadcast(&world, system, None, entries);
    Outcome::Value(Value::None)
}

async fn parse_order(ctx: &Ctx<'_>, call: &Call) -> Option<Order> {
    let f = |i: usize| call.args.get(i).and_then(|v| v.as_f64());
    let vec3 = || Some(Vec3::new(f(0)?, f(1)?, f(2)?));
    let range = |default: f64| f(1).unwrap_or(default).max(0.0);
    Some(match call.method.as_str() {
        "SetSpeedFraction" => Order::SpeedFraction(f(0)?.clamp(0.0, 1.0)),
        "GotoDirection" => Order::Command(Command::Direction(vec3()?)),
        "GotoPoint" => Order::Command(Command::Point(vec3()?)),
        "Stop" => Order::Command(Command::Stop),
        "FollowBall" => Order::Command(Command::Follow {
            target: call.arg_i64(0)?,
            range: range(DEFAULT_FOLLOW_RANGE),
        }),
        "Orbit" => Order::Command(Command::Orbit { target: call.arg_i64(0)?, range: range(0.0) }),
        "WarpToStuff" => {
            let kind = call.arg_text(0)?;
            let id = call.arg_i64(1)?;
            let range = call.kwarg("minRange").and_then(|v| v.as_f64()).unwrap_or(DEFAULT_WARP_RANGE);
            let (at, radius) = warp_target(ctx, &kind, id).await?;
            Order::Command(Command::Warp { target: landing_point(ctx, at, radius + range.max(0.0))?, range: 0.0 })
        }
        _ => return None,
    })
}

/// What WarpToStuff(kind, id) goes to, as its centre and radius: an item in
/// the system, a bookmark, or another character's ship.
async fn warp_target(ctx: &Ctx<'_>, kind: &str, id: i64) -> Option<(Vec3, f64)> {
    let point = |r: &tokio_postgres::Row, i: usize| {
        Some(Vec3::new(float(r, i)?, float(r, i + 1)?, float(r, i + 2)?))
    };
    match kind {
        "item" => {
            if let Some(ball) = ctx.world.lock().parks.get(&ctx.session.solar_system_id?).and_then(|p| p.ball(id)) {
                return Some(ball);
            }
            let sql = r#"SELECT "x","y","z", COALESCE("radius", 0) FROM "mapDenormalize" WHERE "itemID" = $1"#;
            let r = ctx.query_one(sql, &[&(id as i32)]).await?;
            Some((point(&r, 0)?, float(&r, 3).unwrap_or(0.0)))
        }
        "bookmark" => {
            let sql = r#"SELECT b."x", b."y", b."z", d."x", d."y", d."z", COALESCE(d."radius", 0)
                           FROM bookmarks b
                           LEFT JOIN "mapDenormalize" d ON d."itemID" = b."itemID"
                          WHERE b."bookmarkID" = $1 AND b."ownerID" = $2"#;
            let owner = ctx.session.char_id? as i32;
            let r = ctx.query_one(sql, &[&(id as i32), &owner]).await?;
            // A bookmark of a celestial carries the celestial's position.
            match point(&r, 3) {
                Some(at) => Some((at, float(&r, 6).unwrap_or(0.0))),
                None => Some((point(&r, 0)?, 0.0)),
            }
        }
        "char" => {
            let world = ctx.world.lock();
            let park = world.parks.get(&ctx.session.solar_system_id?)?;
            park.ships.values().find(|s| s.pilot.char_id == id).map(|s| (s.pos, s.stats.radius))
        }
        _ => {
            warn!("WarpToStuff to a {kind:?} is not supported");
            None
        }
    }
}

/// Where the warp lands: `reach` (the target's radius plus the warp range)
/// out from `at` towards the ship, plus the ship's own radius, so the
/// overview's surfaceDist reads the range asked for.
///
/// The server works the point out and sends WarpTo a range of 0. Left to
/// destiny, the range is counted from wherever the ship finished aligning,
/// which beside a planet can point under its surface; and a warp of under
/// 100 km is a GotoPoint to the raw point, flying the ship into the planet.
fn landing_point(ctx: &Ctx<'_>, at: Vec3, reach: f64) -> Option<Vec3> {
    let world = ctx.world.lock();
    let park = world.parks.get(&ctx.session.solar_system_id?)?;
    let ship = park.ships.get(&ctx.session.ship_id?)?;
    Some(at + (ship.pos - at).unit() * (reach + ship.stats.radius))
}

// ---- jumps --------------------------------------------------------------------------

pub struct Jump {
    pub solar_system_id: i64,
    pub constellation_id: i64,
    pub region_id: i64,
    pub arrival: Vec3,
}

pub async fn resolve_jump(ctx: &Ctx<'_>, solar_system_id: i64, from_gate: i64, to_gate: i64) -> Option<Jump> {
    let sql = r#"
        SELECT d."solarSystemID", d."constellationID", d."regionID",
               d."x", d."y", d."z", COALESCE(d."radius", 0)
          FROM "mapJumps" j
          JOIN "mapDenormalize" g ON g."itemID" = j."stargateID"
          JOIN "mapDenormalize" d ON d."itemID" = j."celestialID"
         WHERE j."stargateID" = $1 AND j."celestialID" = $2
           AND g."solarSystemID" = $3 AND g."groupID" = $4"#;
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 4] =
        [&(from_gate as i32), &(to_gate as i32), &(solar_system_id as i32), &(GROUP_STARGATE as i32)];
    let r = ctx.query_one(sql, &params).await?;
    let gate = Vec3::new(float(&r, 3)?, float(&r, 4)?, float(&r, 5)?);
    Some(Jump {
        solar_system_id: int(&r, 0)?,
        constellation_id: int(&r, 1)?,
        region_id: int(&r, 2)?,
        arrival: arrival_point(gate, float(&r, 6).unwrap_or(0.0)),
    })
}

const ARRIVAL_CLEARANCE: f64 = 1500.0;

/// Where a ship comes out of a gate: just off its surface, on the sun's side.
pub fn arrival_point(gate: Vec3, gate_radius: f64) -> Vec3 {
    let offset = gate_radius + ARRIVAL_CLEARANCE;
    if gate.len() < offset {
        return gate + Vec3::new(offset, 0.0, 0.0);
    }
    gate - gate.unit() * offset
}

const UNDOCK_ALONG_MIN_COS: f64 = 0.25;

/// The undock point and push of a station: its type's dock entry, heading out
/// along the dock orientation.
///
/// Some types' orientation runs along the hull or back into it (type 56 has
/// its entry out along +Z and its orientation down -Y), which slides the
/// undocking ship into the station; those leave straight out from the centre
/// through the entry instead.
pub async fn undock_point(ctx: &Ctx<'_>, station_id: i64) -> Option<(Vec3, Vec3)> {
    let sql = r#"SELECT d."x", d."y", d."z",
                        st."dockEntryX", st."dockEntryY", st."dockEntryZ",
                        st."dockOrientationX", st."dockOrientationY", st."dockOrientationZ"
                   FROM "staStations" s
                   JOIN "mapDenormalize" d ON d."itemID" = s."stationID"
                   LEFT JOIN "staStationTypes" st ON st."stationTypeID" = s."stationTypeID"
                  WHERE s."stationID" = $1"#;
    let r = ctx.query_one(sql, &[&(station_id as i32)]).await?;
    let v = |i: usize| Vec3::new(float(&r, i).unwrap_or(0.0), float(&r, i + 1).unwrap_or(0.0), float(&r, i + 2).unwrap_or(0.0));
    let (station, entry) = (v(0), v(3));
    Some((station + entry, undock_heading(entry, v(6))))
}

/// See `undock_point`: the orientation if it leads away from the station,
/// within ~75 degrees of straight out (the sound types are within 70).
fn undock_heading(entry: Vec3, orientation: Vec3) -> Vec3 {
    let (out, along) = (entry.unit(), orientation.unit());
    if along != Vec3::ZERO && (out == Vec3::ZERO || out.dot(along) > UNDOCK_ALONG_MIN_COS) {
        along
    } else if out != Vec3::ZERO {
        out
    } else {
        Vec3::new(1.0, 0.0, 0.0)
    }
}

/// Is `id` a station in the park?
pub fn is_station(park: &Park, id: i64) -> bool {
    park.celestial(id).is_some_and(|c| c.group_id == GROUP_STATION)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ballpark::{Pilot, ShipStats};

    #[test]
    fn a_destiny_entry_is_stamp_then_opcode_then_args() {
        let e = entry(7, "Stop", vec![Value::Int(140_000_016)]);
        let Value::Tuple(outer) = &e else { panic!("entry is a tuple") };
        assert_eq!(outer[0], Value::Int(7));
        let Value::Tuple(event) = &outer[1] else { panic!("event is a tuple") };
        assert_eq!(event[0], Value::bytes("Stop"));
        assert_eq!(event[1], Value::Tuple(vec![Value::Int(140_000_016)]));
    }

    fn park_with_moving_ship() -> Park {
        let mut park = Park::new(30002780, Vec::new(), 100);
        let stats = ShipStats::new(37.0, 1_100_000.0, 3.1, 320.0, 2.0);
        let pilot = Pilot {
            type_id: 587,
            group_id: 25,
            name: "Rifter".into(),
            char_id: 140000001,
            corp_id: 1000002,
            alliance_id: None,
            security: -1.5,
            bounty: 0.0,
        };
        park.ships.insert(1, Ship::new(1, 10, stats, pilot, Vec3::ZERO, Vec3::ZERO));
        park.command(1, Command::Orbit { target: 2, range: 500.0 });
        park
    }

    #[test]
    fn set_state_comes_first_and_moving_ships_are_put_back_on_course() {
        let park = park_with_moving_ship();
        let entries = set_state_entries(&park, 1, &[], &[]);
        let opcode = |e: &Value| match e {
            Value::Tuple(t) => match &t[1] {
                Value::Tuple(ev) => ev[0].clone(),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        let names: Vec<Value> = entries.iter().map(opcode).collect();
        assert_eq!(
            names,
            vec![Value::bytes("SetState"), Value::bytes("Orbit"), Value::bytes("SetSpeedFraction")]
        );
    }

    #[test]
    fn a_ship_slim_carries_a_numeric_security_status() {
        let park = park_with_moving_ship();
        let Value::Instance { state, .. } = ship_slim(&park.ships[&1]) else { panic!() };
        assert_eq!(state.get_str("securityStatus"), Some(&Value::Double(-1.5)));
        assert_eq!(state.get_str("allianceID"), Some(&Value::None));
    }

    #[test]
    fn a_jump_lands_outside_the_gate_and_inside_jump_range() {
        let gate = Vec3::new(-3.2e11, 4.1e10, 7.7e11);
        let at = arrival_point(gate, 2960.0);
        let surface = (at - gate).len() - 2960.0;
        assert!((surface - ARRIVAL_CLEARANCE).abs() < 0.01, "surface distance {surface}");
        assert!(at.len() < gate.len(), "on the sun's side");
    }

    #[test]
    fn an_undock_heads_away_from_the_station() {
        // Type 3870: the orientation leads out of the bay, so it is kept.
        let h = undock_heading(Vec3::new(6765.0, 938.0, 0.0), Vec3::new(6947.0, -2257.0, 0.0));
        assert!((h - Vec3::new(6947.0, -2257.0, 0.0).unit()).len() < 1e-9);
        // Type 56: the orientation runs down the hull; leave through the entry.
        let h = undock_heading(Vec3::new(0.0, -198.0, 4800.0), Vec3::new(0.0, -4967.0, 0.0));
        assert!((h - Vec3::new(0.0, -198.0, 4800.0).unit()).len() < 1e-9);
        // Types 58/59: no entry offset, only an orientation.
        assert_eq!(undock_heading(Vec3::ZERO, Vec3::new(0.0, 0.0, -1.0)), Vec3::new(0.0, 0.0, -1.0));
    }
}
