//! `beyonce`, the remote ballpark: what the client sees in space and how its
//! ship moves (michelle.py, docs: destiny/ballpark/michelle).
//!
//! Every solar system with a ship in it has a [`Park`] in the shared world. A
//! movement command updates the park and goes out as a destiny opcode to every
//! client in the system, which then runs the same motion locally. A client that
//! (re)builds its ballpark gets a SetState of the whole park.

use eve_marshal::Value;
use tracing::{debug, warn};

use crate::ballpark::{Celestial, Command, Park, Pilot, Ship, ShipStats, Vec3};
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
    let mut world = ctx.world.lock();
    world
        .parks
        .entry(solar_system_id)
        .or_insert_with(|| Park::new(solar_system_id, celestials, destiny::tick_now()));
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

/// The ship's flight characteristics and its pilot, from its type and character.
pub async fn load_ship(ctx: &Ctx<'_>, ship_id: i64) -> Option<(ShipStats, Pilot)> {
    let sql = r#"
        SELECT e."typeID", COALESCE(NULLIF(e."itemName", ''), t."typeName"),
               COALESCE(t."radius", 0), COALESCE(t."mass", 0),
               COALESCE((SELECT COALESCE(a."valueFloat", a."valueInt") FROM "dgmTypeAttributes" a
                          WHERE a."typeID" = e."typeID" AND a."attributeID" = 70), 1),
               COALESCE((SELECT COALESCE(a."valueFloat", a."valueInt") FROM "dgmTypeAttributes" a
                          WHERE a."typeID" = e."typeID" AND a."attributeID" = 37), 0),
               COALESCE((SELECT COALESCE(a."valueFloat", a."valueInt") FROM "dgmTypeAttributes" a
                          WHERE a."typeID" = e."typeID" AND a."attributeID" = 600), 1),
               c."characterID", c."corporationID", co."allianceID",
               COALESCE(c."securityRating", 0), COALESCE(c."bounty", 0)
          FROM entity e
          JOIN "invTypes" t ON t."typeID" = e."typeID"
          LEFT JOIN "character_" c ON c."characterID" = $2
          LEFT JOIN corporation co ON co."corporationID" = c."corporationID"
         WHERE e."itemID" = $1"#;
    let char_id = ctx.session.char_id.unwrap_or(0) as i32;
    let r = ctx.query_one(sql, &[&(ship_id as i32), &char_id]).await?;
    let f = |i: usize| float(&r, i).unwrap_or(0.0);
    let stats = ShipStats::new(f(2), f(3), f(4), f(5), f(6));
    let pilot = Pilot {
        type_id: int(&r, 0).unwrap_or(0),
        name: text(&r, 1).unwrap_or_default(),
        char_id: int(&r, 7).unwrap_or(0),
        corp_id: int(&r, 8).unwrap_or(0),
        alliance_id: int(&r, 9).filter(|a| *a != 0),
        security: f(10),
        bounty: f(11),
    };
    Some((stats, pilot))
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
        flags: flags::SHIP,
        mass: ship.stats.mass,
        corp_id: ship.pilot.corp_id as i32,
        alliance_id: ship.pilot.alliance_id.unwrap_or(-1) as i32,
        max_velocity: ship.stats.max_velocity,
        velocity: ship.vel.tuple(),
        agility: ship.stats.agility,
        speed_fraction: ship.speed_fraction,
        name: ship.pilot.name.clone(),
    }
}

fn slim(fields: Vec<(&str, Value)>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("foo.SlimItem")),
        state: Box::new(Value::Dict(fields.into_iter().map(|(k, v)| (Value::bytes(k), v)).collect())),
    }
}

/// groupID/categoryID are left out on purpose: the client derives them from typeID.
fn ship_slim(ship: &Ship) -> Value {
    let p = &ship.pilot;
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

/// [shield (fraction, tau), armor, structure]; everything is undamaged.
fn full_health() -> Value {
    Value::List(vec![
        Value::Tuple(vec![Value::Double(1.0), Value::Double(1.0)]),
        Value::Double(1.0),
        Value::Double(1.0),
    ])
}

/// The destiny opcode for a ship's current command.
fn command_entry(ship: &Ship, tick: i64) -> Value {
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
        Command::Warp { target, range } => {
            let mut args = point(target);
            args.push(Value::Double(*range));
            entry(tick, "WarpTo", args)
        }
    }
}

/// The opcodes that put a ship back on its current course, for a client that
/// has just been given the ship standing still.
fn course_entries(ship: &Ship, tick: i64) -> Vec<Value> {
    if ship.command == Command::Stop {
        return Vec::new();
    }
    let speed = entry(tick, "SetSpeedFraction", vec![Value::Int(ship.id), Value::Double(ship.speed_fraction)]);
    vec![command_entry(ship, tick), speed]
}

/// SetState for `ego`: the whole park, then every moving ship's course.
pub fn set_state_entries(park: &Park, ego: i64, station_owners: &[(i64, i64)]) -> Vec<Value> {
    const DRONE_HEADER: &[&str] =
        &["droneID", "ownerID", "controllerID", "activityState", "typeID", "controllerOwnerID"];
    let mut balls: Vec<Ball> = park
        .celestials
        .iter()
        .map(|c| Ball::celestial(c.id, c.radius, c.pos.tuple(), &c.name))
        .collect();
    let mut slims: Vec<Value> = park.celestials.iter().map(|c| celestial_slim(c, station_owners)).collect();
    let mut damage = Vec::new();
    for ship in park.ships.values() {
        balls.push(ship_ball(ship));
        slims.push(ship_slim(ship));
        damage.push((Value::Int(ship.id), full_health()));
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
        ("aggressors", Value::Dict(Vec::new())),
        ("droneState", rowset(DRONE_HEADER, Vec::new())),
        ("gangCorps", Value::List(Vec::new())),
        ("charGangs", Value::List(Vec::new())),
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
        Value::Dict(vec![(Value::Int(ship.id), full_health())]),
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

/// Takes a ship out of its system's park and out of everyone's view; returns it.
pub fn remove_ship(world: &mut WorldState, system: i64, ship_id: i64) -> Option<Ship> {
    let park = world.parks.get_mut(&system)?;
    park.advance(destiny::tick_now());
    let ship = park.ships.remove(&ship_id)?;
    let entries = remove_ball_entries(park.tick, ship_id);
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
    let entries = set_state_entries(park, ship_id, &owners);
    debug!("SetState: {} ball(s), ego {ship_id}", park.celestials.len() + park.ships.len());
    broadcast(&world, system, Some(ctx.client_id), entries);
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
    let mut world = ctx.world.lock();
    let Some(park) = world.parks.get_mut(&system) else {
        return Outcome::Unknown(format!("beyonce::{}: no ballpark for {system}", call.method));
    };
    park.advance(destiny::tick_now());
    let tick = park.tick;
    match park.ships.get(&ship_id) {
        None => {
            return Outcome::Unknown(format!("beyonce::{}: ship {ship_id} is not in space", call.method))
        }
        // destiny ignores course changes mid-warp; so does the server.
        Some(ship) if ship.in_warp() => return Outcome::Value(Value::None),
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
            park.command(ship_id, command);
            (vec![command_entry(&park.ships[&ship_id], tick)], what)
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
            let target = warp_target(ctx, &kind, id).await?;
            Order::Command(Command::Warp { target, range })
        }
        _ => return None,
    })
}

/// Where WarpToStuff(kind, id) goes: an item in the system, a bookmark, or
/// another character's ship.
async fn warp_target(ctx: &Ctx<'_>, kind: &str, id: i64) -> Option<Vec3> {
    let point = |r: &tokio_postgres::Row, i: usize| {
        Some(Vec3::new(float(r, i)?, float(r, i + 1)?, float(r, i + 2)?))
    };
    match kind {
        "item" => {
            let sql = r#"SELECT "x","y","z" FROM "mapDenormalize" WHERE "itemID" = $1"#;
            point(&ctx.query_one(sql, &[&(id as i32)]).await?, 0)
        }
        "bookmark" => {
            let sql = r#"SELECT b."x", b."y", b."z", d."x", d."y", d."z"
                           FROM bookmarks b
                           LEFT JOIN "mapDenormalize" d ON d."itemID" = b."itemID"
                          WHERE b."bookmarkID" = $1 AND b."ownerID" = $2"#;
            let owner = ctx.session.char_id? as i32;
            let r = ctx.query_one(sql, &[&(id as i32), &owner]).await?;
            // A bookmark of a celestial carries the celestial's position.
            point(&r, 3).or_else(|| point(&r, 0))
        }
        "char" => {
            let world = ctx.world.lock();
            let park = world.parks.get(&ctx.session.solar_system_id?)?;
            park.ships.values().find(|s| s.pilot.char_id == id).map(|s| s.pos)
        }
        _ => {
            warn!("WarpToStuff to a {kind:?} is not supported");
            None
        }
    }
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

/// The undock point and push of a station: its type's dock entry, heading out
/// along the dock orientation.
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
    let station = v(0);
    let mut heading = v(6).unit();
    if heading == Vec3::ZERO {
        heading = Vec3::new(1.0, 0.0, 0.0);
    }
    Some((station + v(3), heading))
}

/// Is `id` a station in the park?
pub fn is_station(park: &Park, id: i64) -> bool {
    park.celestial(id).is_some_and(|c| c.group_id == GROUP_STATION)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let entries = set_state_entries(&park, 1, &[]);
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
}
