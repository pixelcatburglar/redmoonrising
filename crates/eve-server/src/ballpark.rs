//! Server-side destiny: what every ship in a solar system is doing and where it is.
//!
//! The client runs destiny.dll on the commands we send it, and it never hears
//! a position from us again until the next SetState or AddBalls: those, the
//! other players who see a ship, and every range check (docking, jumping,
//! locking, looting) read this simulation. So it is a port of destiny's
//! Ballpark::Evolve, not an approximation: wherever it drifts from the client,
//! two players see a ship in two places.
//!
//! One tick per second (const.simulationTimeStep). Each tick, as Evolve does it:
//!
//! 1. every ship's mode picks a thrust (a goal velocity divided by tau);
//! 2. every massive ship works out whether it will touch another massive ball
//!    during the tick, and adds a thrust that bounces it off;
//! 3. every ship is integrated exactly over the tick: velocity converges on
//!    tau * thrust with tau = mass * agility / friction; or, in warp, follows
//!    the warp's closed-form profile.
//!
//! Addresses in the notes are destiny.dll 2.10.3913.

use std::collections::BTreeMap;
use std::ops::{Add, Mul, Sub};
use std::sync::Arc;

use crate::combat::{Fighter, Rng};
use crate::npc::{Belt, Brain, Missile, Rock, Wildlife};

/// Ticks a park evolves at most per request; a park left alone longer than this
/// skips ahead instead (its ships have long since stopped).
const MAX_CATCH_UP: i64 = 3600;

/// Seconds per tick (Ballpark+0x60).
const DT: f64 = 1.0;
/// Ballpark.friction, "Friction coefficient of space": tau = inertia / friction.
const FRICTION: f64 = 1.0e6;
/// What a ball that is not free weighs, for collisions (0x1005a290).
const IMMOVABLE: f64 = 1.0e34;
/// How far ahead GotoDirection puts its point (0x1005a620).
const GOTO_DIRECTION_REACH: f64 = 1.0e17;
/// STOP bleeds off vertical velocity each tick: (vy - 0.07 vy) / 1.07.
const STOP_LEVELLING: f64 = 0.07;
const STOP_LEVELLING_DIV: f64 = 1.07;
/// Orbit: the reference axis turns by speed * 0.01 / radius radians a tick,
/// and wobbles at 0.7 of that around the ball's own ID.
const ORBIT_TURN: f64 = 0.01;
const ORBIT_WOBBLE: f64 = 0.7;
/// Orbit: how sharply the circle pulls, exp((R - d)^2 / -40000).
const ORBIT_PULL: f64 = -40_000.0;
/// Collision thrust is scaled down by this (0x1005a268).
const BOUNCE: f64 = 0.85;
/// WarpTo closer than this (squared, 100 km) is a GotoPoint (0x10059548).
const MIN_WARP_DISTANCE_SQ: f64 = 1.0e10;
/// A warp starts once 1 - cos(heading, target) is below this and the speed
/// is over 0.75 of the maximum (squared: 0.5625).
const WARP_ALIGN: f64 = 0.01;
const WARP_ENTRY_SPEED_SQ: f64 = 0.5625;
/// Warp speed per point of warp factor: 0.1 AU/s (0x1005a598).
const WARP_FACTOR_SPEED: f64 = 14_959_787_070.0;
/// The warp's top speed is also limited to 1.5 times its distance per second.
const WARP_DISTANCE_SPEED: f64 = 1.5;
/// Warp factor of a warpSpeedMultiplier-1 ship: 3 AU/s.
const BASE_WARP_FACTOR: f64 = 30.0;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl Vec3 {
    pub const ZERO: Vec3 = Vec3 { x: 0.0, y: 0.0, z: 0.0 };

    pub fn new(x: f64, y: f64, z: f64) -> Self {
        Vec3 { x, y, z }
    }
    pub fn len(self) -> f64 {
        self.len_sq().sqrt()
    }
    pub fn len_sq(self) -> f64 {
        self.dot(self)
    }
    pub fn dot(self, o: Vec3) -> f64 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }
    pub fn cross(self, o: Vec3) -> Vec3 {
        Vec3::new(self.y * o.z - o.y * self.z, o.x * self.z - self.x * o.z, o.y * self.x - self.y * o.x)
    }
    /// The unit vector; a zero vector stays zero (destiny's Normalize, 0x100014e0).
    pub fn unit(self) -> Vec3 {
        let l = self.len();
        if l == 0.0 { self } else { self * (1.0 / l) }
    }
    pub fn tuple(self) -> (f64, f64, f64) {
        (self.x, self.y, self.z)
    }
}

impl Add for Vec3 {
    type Output = Vec3;
    fn add(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}
impl Sub for Vec3 {
    type Output = Vec3;
    fn sub(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}
impl Mul<f64> for Vec3 {
    type Output = Vec3;
    fn mul(self, k: f64) -> Vec3 {
        Vec3::new(self.x * k, self.y * k, self.z * k)
    }
}

const GROUP_STATION: i64 = 15;

/// A fixed object in the system: sun, planet, moon, belt, stargate, station.
#[derive(Debug, Clone)]
pub struct Celestial {
    pub id: i64,
    pub type_id: i64,
    pub group_id: i64,
    pub name: String,
    pub pos: Vec3,
    pub radius: f64,
    /// For a stargate: (destination solarSystemID, destination gate).
    pub jump: Option<(i64, i64)>,
}

impl Celestial {
    /// Whether ships bounce off it. Not a station: its undock point lies
    /// inside its radius, so a massive station would throw every ship that
    /// leaves it.
    pub fn massive(&self) -> bool {
        self.group_id != GROUP_STATION
    }
}

/// What a ship's type makes it do.
#[derive(Debug, Clone, Copy)]
pub struct ShipStats {
    pub radius: f64,
    pub mass: f64,
    pub agility: f64,
    pub max_velocity: f64,
    /// WarpTo's warp factor: tenths of an AU per second.
    pub warp_factor: i64,
}

impl ShipStats {
    pub fn new(radius: f64, mass: f64, agility: f64, max_velocity: f64, warp_multiplier: f64) -> Self {
        let multiplier = if warp_multiplier > 0.0 { warp_multiplier } else { 1.0 };
        ShipStats {
            radius,
            mass,
            agility,
            max_velocity,
            warp_factor: ((BASE_WARP_FACTOR * multiplier).round() as i64).max(1),
        }
    }

    /// mass * agility: what thrust is divided by.
    fn inertia(&self) -> f64 {
        (self.mass * self.agility).max(f64::MIN_POSITIVE)
    }

    /// Seconds for the velocity to close 63% of the gap to its target.
    #[cfg(test)]
    pub fn tau(&self) -> f64 {
        self.inertia() / FRICTION
    }
}

/// Who flies a ship, for its SlimItem.
#[derive(Debug, Clone)]
pub struct Pilot {
    pub type_id: i64,
    pub group_id: i64,
    pub name: String,
    pub char_id: i64,
    pub corp_id: i64,
    pub alliance_id: Option<i64>,
    pub security: f64,
    pub bounty: f64,
}

/// The movement command a ship is executing, as the client was told it.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Stop,
    Direction(Vec3),
    Point(Vec3),
    Follow { target: i64, range: f64 },
    Orbit { target: i64, range: f64 },
    /// Warp towards `target`, stopping `range` short of it.
    Warp { target: Vec3, range: f64 },
}

#[derive(Debug, Clone, Copy)]
enum WarpState {
    /// Turning and speeding up towards the warp point.
    Aligning,
    /// On the way: `start` is the tick the warp began, `distance` from where
    /// it began to `exit`.
    InWarp { exit: Vec3, distance: f64, start: i64 },
}

#[derive(Debug, Clone)]
pub struct Ship {
    pub id: i64,
    pub client_id: i64,
    pub stats: ShipStats,
    pub pilot: Pilot,
    pub pos: Vec3,
    pub vel: Vec3,
    pub speed_fraction: f64,
    pub command: Command,
    /// Where GOTO steers (the point, the far point of a direction, the warp
    /// point); ball+0xd0.
    goal: Vec3,
    /// The velocity a tick ago, which collisions start from (ball+0x90).
    prev_vel: Vec3,
    warp: Option<WarpState>,
    /// Hit points, locks and running modules; None for a ship that cannot fight.
    pub fight: Option<Fighter>,
    /// What an NPC thinks; None for a player's ship.
    pub npc: Option<Brain>,
    /// A player's drone: whose, and what it was told to do.
    pub drone: Option<crate::drones::Drone>,
    /// The shield harmonic its pilot set (SetShipPassword); -1 for none.
    pub harmonic: i64,
}

impl Ship {
    pub fn new(id: i64, client_id: i64, stats: ShipStats, pilot: Pilot, pos: Vec3, vel: Vec3) -> Self {
        Ship {
            id,
            client_id,
            stats,
            pilot,
            pos,
            vel,
            speed_fraction: 1.0,
            command: Command::Stop,
            goal: pos,
            // A new ball starts with no previous velocity (0x1000cf90).
            prev_vel: Vec3::ZERO,
            warp: None,
            fight: None,
            npc: None,
            drone: None,
            harmonic: crate::destiny::NO_HARMONIC,
        }
    }

    pub fn in_warp(&self) -> bool {
        matches!(self.warp, Some(WarpState::InWarp { .. }))
    }

    /// Aligning for or in warp.
    pub fn warping(&self) -> bool {
        self.warp.is_some()
    }

    pub fn is_player(&self) -> bool {
        self.npc.is_none() && self.drone.is_none()
    }

    /// Whether others bounce off it: always, but not in warp.
    pub fn massive(&self) -> bool {
        !self.in_warp()
    }

    fn cruise(&self) -> f64 {
        self.speed_fraction * self.stats.max_velocity
    }
}

/// One solar system's moving objects.
#[derive(Debug, Clone)]
pub struct Park {
    pub solar_system_id: i64,
    /// The tick the ships' state is valid for.
    pub tick: i64,
    pub celestials: Vec<Celestial>,
    pub ships: BTreeMap<i64, Ship>,
    /// Asteroids, by ID.
    pub rocks: BTreeMap<i64, Rock>,
    /// Cargo containers floating here, by item ID.
    pub cans: BTreeMap<i64, crate::loot::Can>,
    /// Starbase structures, by item ID.
    pub structures: BTreeMap<i64, crate::pos::Structure>,
    /// Aggression and crime here.
    pub watch: crate::concord::Watch,
    pub belts: Vec<Belt>,
    /// Missiles on their way to a target.
    pub missiles: Vec<Missile>,
    /// What lives in this system's belts.
    pub wildlife: Arc<Wildlife>,
    pub rng: Rng,
}

impl Park {
    pub fn new(solar_system_id: i64, celestials: Vec<Celestial>, tick: i64) -> Self {
        Park {
            solar_system_id,
            tick,
            celestials,
            ships: BTreeMap::new(),
            rocks: BTreeMap::new(),
            cans: BTreeMap::new(),
            structures: BTreeMap::new(),
            watch: Default::default(),
            belts: Vec::new(),
            missiles: Vec::new(),
            wildlife: Arc::new(Wildlife::default()),
            rng: Rng::seeded(solar_system_id as u64 ^ tick as u64),
        }
    }

    pub fn celestial(&self, id: i64) -> Option<&Celestial> {
        self.celestials.iter().find(|c| c.id == id)
    }

    /// Position and radius of any ball in the park.
    pub fn ball(&self, id: i64) -> Option<(Vec3, f64)> {
        if let Some(s) = self.ships.get(&id) {
            return Some((s.pos, s.stats.radius));
        }
        if let Some(r) = self.rocks.get(&id) {
            return Some((r.pos, r.radius));
        }
        if let Some(c) = self.cans.get(&id) {
            return Some((c.pos, c.radius));
        }
        if let Some(s) = self.structures.get(&id) {
            return Some((s.pos, s.radius));
        }
        self.celestial(id).map(|c| (c.pos, c.radius))
    }

    /// Players' ships.
    pub fn players(&self) -> impl Iterator<Item = &Ship> {
        self.ships.values().filter(|s| s.is_player())
    }

    /// Surface-to-surface distance between two balls.
    pub fn surface_distance(&self, a: i64, b: i64) -> Option<f64> {
        let (pa, ra) = self.ball(a)?;
        let (pb, rb) = self.ball(b)?;
        Some(((pa - pb).len() - ra - rb).max(0.0))
    }

    /// Brings every ship forward to `tick`.
    pub fn advance(&mut self, tick: i64) {
        if tick <= self.tick {
            return;
        }
        self.tick = self.tick.max(tick - MAX_CATCH_UP);
        while self.tick < tick {
            self.step();
        }
    }

    /// Gives a ship a new command at the park's current tick, as destiny's
    /// FollowBall, Orbit, GotoPoint, GotoDirection, WarpTo and Stop take it.
    pub fn command(&mut self, ship_id: i64, command: Command) {
        let Some(ship) = self.ships.get_mut(&ship_id) else { return };
        let command = match command {
            // Stop on a stopped ship changes nothing (0x10017800).
            Command::Stop if ship.command == Command::Stop && ship.warp.is_none() => return,
            // Too close to warp: destiny goes there instead (0x10017bb0).
            Command::Warp { target, .. } if (target - ship.pos).len_sq() < MIN_WARP_DISTANCE_SQ => Command::Point(target),
            other => other,
        };
        ship.warp = None;
        match &command {
            Command::Point(p) => ship.goal = *p,
            Command::Direction(d) => ship.goal = ship.pos + d.unit() * GOTO_DIRECTION_REACH,
            Command::Warp { target, .. } => {
                ship.goal = *target;
                ship.warp = Some(WarpState::Aligning);
            }
            _ => {}
        }
        // A GOTO with the throttle at zero opens it (0x10017730).
        if matches!(command, Command::Point(_) | Command::Direction(_)) && ship.speed_fraction == 0.0 {
            ship.speed_fraction = 1.0;
        }
        ship.command = command;
    }

    pub fn set_speed_fraction(&mut self, ship_id: i64, fraction: f64) {
        if let Some(ship) = self.ships.get_mut(&ship_id) {
            ship.speed_fraction = fraction.clamp(0.0, 1.0);
        }
    }

    /// Seconds until `ship_id` is within `range` of `target`'s surface, if that
    /// happens within `limit` seconds on its current course.
    pub fn eta_within(&self, ship_id: i64, target: i64, range: f64, limit: i64) -> Option<i64> {
        let mut future = self.clone();
        for t in 0..=limit {
            if future.surface_distance(ship_id, target)? <= range {
                return Some(t);
            }
            future.step();
        }
        None
    }

    /// One tick of Ballpark::Evolve (0x100189c0).
    fn step(&mut self) {
        // The clock destiny reads during the tick: the stamp, as a C int.
        let now = crate::destiny::stamp(self.tick) as i32;
        let thrust = self.thrusts(now);
        let bounce = self.bounces(&thrust);
        for ship in self.ships.values_mut() {
            let push = thrust.get(&ship.id).copied().unwrap_or(Vec3::ZERO) + bounce.get(&ship.id).copied().unwrap_or(Vec3::ZERO);
            move_ship(ship, push, now);
        }
        self.tick += 1;
    }

    /// Step 1: each ship's thrust from its mode. Modes can change on the way:
    /// a lost target stops a follower, an aligned ship enters warp (and its
    /// followers let go of it).
    fn thrusts(&mut self, now: i32) -> BTreeMap<i64, Vec3> {
        let targets: BTreeMap<i64, (Vec3, f64)> = self
            .ships
            .values()
            .map(|s| (s.id, (s.pos, s.stats.radius)))
            .chain(self.rocks.values().map(|r| (r.id, (r.pos, r.radius))))
            .chain(self.cans.values().map(|c| (c.id, (c.pos, c.radius))))
            .chain(self.structures.values().map(|s| (s.id, (s.pos, s.radius))))
            .chain(self.celestials.iter().map(|c| (c.id, (c.pos, c.radius))))
            .collect();
        let mut thrust = BTreeMap::new();
        // destiny walks its balls in ID order, and a warp that starts changes
        // its followers on the spot, so later ones see it the same tick.
        let ids: Vec<i64> = self.ships.keys().copied().collect();
        for id in ids {
            let Some(ship) = self.ships.get_mut(&id) else { continue };
            let mut warped = false;
            let push = match ship.command.clone() {
                Command::Stop => {
                    ship.vel.y = (ship.vel.y - ship.vel.y * STOP_LEVELLING) / STOP_LEVELLING_DIV;
                    Vec3::ZERO
                }
                Command::Point(_) | Command::Direction(_) => steer(ship, ship.goal),
                Command::Follow { target, range } => match targets.get(&target) {
                    Some(&(at, radius)) => {
                        ship.goal = follow_point(ship, at, radius + ship.stats.radius + range);
                        steer(ship, ship.goal)
                    }
                    None => lose_target(ship),
                },
                Command::Orbit { target, range } => match targets.get(&target) {
                    Some(&(at, radius)) => orbit(ship, at, radius + ship.stats.radius + range, now),
                    None => lose_target(ship),
                },
                Command::Warp { range, .. } => match ship.warp {
                    Some(WarpState::Aligning) if aligned(ship) => {
                        enter_warp(ship, range, now);
                        warped = true;
                        Vec3::ZERO
                    }
                    Some(WarpState::Aligning) => steer(ship, ship.goal),
                    _ => Vec3::ZERO,
                },
            };
            thrust.insert(id, push);
            if warped {
                self.let_go_of(id);
            }
        }
        thrust
    }

    /// `warped` went into warp: whoever was following it lets go (0x10017e80).
    /// An orbit carries on in a straight line; anything else stops.
    fn let_go_of(&mut self, warped: i64) {
        for ship in self.ships.values_mut() {
            match ship.command {
                Command::Orbit { target, .. } if target == warped => {
                    let heading = ship.vel;
                    ship.goal = ship.pos + heading.unit() * GOTO_DIRECTION_REACH;
                    ship.command = Command::Direction(heading);
                }
                Command::Follow { target, .. } if target == warped => ship.command = Command::Stop,
                _ => {}
            }
        }
    }

    /// Step 2: the extra thrust each massive ship needs this tick to bounce off
    /// whatever massive ball it is about to touch (0x10017a10, 0x10015b20).
    fn bounces(&self, thrust: &BTreeMap<i64, Vec3>) -> BTreeMap<i64, Vec3> {
        let mut bodies: Vec<Body> = self
            .ships
            .values()
            .filter(|s| s.massive())
            .map(|s| Body {
                id: s.id,
                pos: s.pos,
                vel: s.vel,
                thrust: thrust.get(&s.id).copied().unwrap_or(Vec3::ZERO),
                radius: s.stats.radius,
                inertia: s.stats.inertia(),
                mass: s.stats.mass,
                free: true,
                key: (s.harmonic, s.pilot.corp_id, s.pilot.alliance_id.unwrap_or(-1)),
                field: None,
            })
            .collect();
        let movers = bodies.len();
        let fixed = |id: i64, pos: Vec3, radius: f64| Body {
            id,
            pos,
            vel: Vec3::ZERO,
            thrust: Vec3::ZERO,
            radius,
            inertia: IMMOVABLE,
            mass: IMMOVABLE,
            free: false,
            key: (-1, -1, -1),
            field: None,
        };
        bodies.extend(self.rocks.values().map(|r| fixed(r.id, r.pos, r.radius)));
        bodies.extend(self.celestials.iter().filter(|c| c.massive()).map(|c| fixed(c.id, c.pos, c.radius)));
        bodies.extend(self.structures.values().filter(|s| s.fixed()).map(|s| fixed(s.id, s.pos, s.radius)));
        for s in self.structures.values() {
            if let Some(f) = &s.field {
                bodies.push(Body { field: Some((f.harmonic, f.corp_id, f.alliance_id)), ..fixed(f.id, s.pos, f.radius) });
            }
        }

        let mut out = BTreeMap::new();
        for a in &bodies[..movers] {
            let prev_vel = self.ships[&a.id].prev_vel;
            let mut best: Option<(f64, Vec3)> = None;
            for b in bodies.iter().filter(|b| b.id != a.id && !b.field.is_some_and(|f| passes(f, a.key))) {
                let Some((toi, push)) = bounce(a, prev_vel, b) else { continue };
                let push = push * BOUNCE;
                // The latest contact of the tick wins; of two at the same
                // moment, the harder push.
                best = match best {
                    Some((t, p)) if toi < t || (toi == t && push.len_sq() <= p.len_sq()) => Some((t, p)),
                    _ => Some((toi, push)),
                };
            }
            if let Some((_, push)) = best {
                out.insert(a.id, push);
            }
        }
        out
    }
}

/// A ball as a collision sees it.
struct Body {
    id: i64,
    pos: Vec3,
    vel: Vec3,
    thrust: Vec3,
    radius: f64,
    /// mass * agility, or IMMOVABLE.
    inertia: f64,
    mass: f64,
    free: bool,
    /// Its (harmonic, corporation, alliance), for force fields to test.
    key: (i64, i64, i64),
    /// A force field's (harmonic, corporation, alliance); -1 where unset.
    field: Option<(i64, i64, i64)>,
}

/// Whether a ball keyed (harmonic, corporation, alliance) passes through a
/// force field: the master harmonic, or a matching harmonic, corporation or
/// alliance (destiny 2.10 0x10028d40).
pub fn passes((harmonic, corp, alliance): (i64, i64, i64), (h, c, a): (i64, i64, i64)) -> bool {
    h == -2 || (harmonic != -1 && harmonic == h) || (corp != -1 && corp == c) || (alliance != -1 && alliance == a)
}

/// Exact integration of v' = (tau * thrust - v) / tau over `dt` (0x1000dda0):
/// where a ball at `pos`, moving at `vel`, ends up, and how fast.
fn drift(pos: Vec3, vel: Vec3, thrust: Vec3, inertia: f64, dt: f64) -> (Vec3, Vec3) {
    if inertia >= IMMOVABLE {
        return (pos + vel * dt, vel);
    }
    let tau = inertia / FRICTION;
    let goal = thrust * tau;
    let k = (-dt / tau).exp();
    (pos + goal * dt + (vel - goal) * (tau * (1.0 - k)), goal + (vel - goal) * k)
}

/// Full thrust at a point, easing off over the last tick's worth of travel
/// (0x1000dc70).
fn steer(ship: &Ship, point: Vec3) -> Vec3 {
    let to = point - ship.pos;
    let dist_sq = to.len_sq();
    let cruise = ship.cruise();
    let near = DT * cruise + ship.stats.radius;
    let mut accel = FRICTION * cruise / ship.stats.inertia();
    if dist_sq < near * near {
        let k = dist_sq / (near * near);
        accel *= k * k;
    }
    to.unit() * accel
}

/// FOLLOW's goal: `reach` from the target's centre, on the line towards us.
fn follow_point(ship: &Ship, at: Vec3, reach: f64) -> Vec3 {
    let off = ship.pos - at;
    let dist = off.len();
    if dist == 0.0 {
        return at + Vec3::new(reach, 0.0, 0.0);
    }
    at + off * reach * (1.0 / dist)
}

/// The followed ball is gone: destiny stops the follower.
fn lose_target(ship: &mut Ship) -> Vec3 {
    ship.command = Command::Stop;
    ship.vel.y = (ship.vel.y - ship.vel.y * STOP_LEVELLING) / STOP_LEVELLING_DIV;
    Vec3::ZERO
}

/// ORBIT (Evolve, case 4): head for the tangent of the orbit circle, blended
/// with a sideways axis that drifts with time and the ship's ID, at full
/// thrust.
fn orbit(ship: &Ship, at: Vec3, reach: f64, now: i32) -> Vec3 {
    let cruise = ship.cruise();
    let accel = FRICTION * cruise / ship.stats.inertia();
    let to = at - ship.pos;
    let dist = to.len();
    let toward = to.unit();
    let turn = if reach != 0.0 { cruise * DT * ORBIT_TURN / reach } else { 0.0 };
    let theta = f64::from(now) * turn;
    let phi = theta * ORBIT_WOBBLE + f64::from(ship.id as i32);
    let axis = Vec3::new(theta.cos() * phi.cos(), phi.sin(), theta.sin() * phi.cos());
    let side = axis.cross(toward).unit();
    let gap = dist * dist - reach * reach;
    let mut heading = toward;
    if gap >= 0.0 {
        heading = (toward * (gap / dist) + side * (gap.sqrt() * reach / dist)).unit();
    }
    let pull = ((reach - dist) * (reach - dist) / ORBIT_PULL).exp();
    let lean = side.dot(heading * -1.0);
    let room = (lean * lean - 1.0) * pull * pull + 1.0;
    let mut along = if room > 0.0 { room.sqrt() + pull * lean } else { pull * lean };
    if dist - reach < 0.0 {
        along = -along;
    }
    (side * pull + heading * along) * accel
}

/// Lined up and fast enough to warp (Evolve, case 3).
fn aligned(ship: &Ship) -> bool {
    let to = (ship.goal - ship.pos).unit();
    let off = (1.0 - to.dot(ship.vel.unit())).abs();
    let v = ship.stats.max_velocity;
    off < WARP_ALIGN && ship.vel.len_sq() > v * v * WARP_ENTRY_SPEED_SQ
}

/// The warp begins (0x100186b0): it ends `range` short of the warp point, on
/// our side of it. destiny keeps the range as a float.
fn enter_warp(ship: &mut Ship, range: f64, now: i32) {
    let exit = ship.goal + (ship.pos - ship.goal).unit() * f64::from(range as f32);
    ship.warp = Some(WarpState::InWarp { exit, distance: (exit - ship.pos).len(), start: i64::from(now) });
}

/// Step 3 for one ship: integrate it, or carry it along its warp.
fn move_ship(ship: &mut Ship, push: Vec3, now: i32) {
    ship.prev_vel = ship.vel;
    if let Some(WarpState::InWarp { exit, distance, start }) = ship.warp {
        warp_along(ship, exit, distance, (i64::from(now) - start) as f64 * DT);
        return;
    }
    let (pos, vel) = drift(ship.pos, ship.vel, push, ship.stats.inertia(), DT);
    ship.pos = pos;
    ship.vel = vel;
}

/// Where a warp `t` seconds old has the ship (0x10017170): out along e^3t,
/// cruise, then in along e^-(t - slowing); it drops out when slower than the
/// ship's own top speed, still moving, and coasts to a stop.
fn warp_along(ship: &mut Ship, exit: Vec3, distance: f64, t: f64) {
    let top = (ship.stats.warp_factor as f64 * WARP_FACTOR_SPEED).min(distance * WARP_DISTANCE_SPEED);
    let cruising = (top / 3.0).ln() / 3.0;
    let slowing = (3.0 * distance + top * ((top / 3.0).ln() - 2.0)) / (3.0 * top);
    let back = (ship.pos - exit).unit();
    let (left, speed) = if t < cruising {
        let gone = (3.0 * t).exp();
        (distance - gone, 3.0 * gone)
    } else if t < slowing {
        (distance - top * (t - cruising + 1.0 / 3.0), top)
    } else {
        let x = ((3.0 * (distance + top * cruising) - (3.0 * top * t + 2.0 * top)) / top) / 3.0;
        let gone = (3.0 * distance - top * x.exp()) / 3.0;
        (distance - gone, (x.exp() * top / 3.0).abs())
    };
    ship.pos = exit + back * left;
    ship.vel = back * -speed;
    if t >= slowing && speed < ship.stats.max_velocity {
        ship.warp = None;
        ship.command = Command::Stop;
    }
}

/// Whether `a` touches `b` during the coming tick, and if so when (a fraction
/// of the tick) and the thrust that takes `a` off it (0x10015b20).
fn bounce(a: &Body, a_prev_vel: Vec3, b: &Body) -> Option<(f64, Vec3)> {
    let reach = a.radius + b.radius;
    let (a1, _) = drift(a.pos, a.vel, a.thrust, a.inertia, DT);
    let (b1, _) = drift(b.pos, b.vel, b.thrust, b.inertia, DT);
    let apart0 = a.pos - b.pos;
    let closing = (a1 - a.pos) - (b1 - b.pos);
    let dist_sq = apart0.len_sq();
    let clear = reach * reach <= dist_sq;
    let toi = match roots(closing.len_sq(), 2.0 * closing.dot(apart0), dist_sq - reach * reach) {
        Some((late, early)) => {
            let mut toi = -1.0;
            if (0.0..=1.0).contains(&late) {
                toi = late;
            }
            if (0.0..=1.0).contains(&early) && early < late {
                toi = early;
            }
            if !clear {
                0.0
            } else if toi < 0.0 {
                return None;
            } else {
                toi
            }
        }
        None if clear => return None,
        None => 0.0,
    };

    let push = if toi <= 0.0 {
        // Already overlapping: shove apart along the line between the
        // centres, a's share by weight.
        let normal = if dist_sq == 0.0 {
            Vec3::new(if a.id > b.id { 1.0 } else { -1.0 }, 0.0, 0.0)
        } else {
            (b.pos - a.pos).unit()
        };
        let share = b.inertia / (b.inertia + a.inertia) * (reach - dist_sq.sqrt());
        let own = normal * normal.dot(a.thrust);
        let speed = -(share / ((1.0 / (DT * FRICTION + a.inertia)) * DT * a.inertia));
        (normal * speed - a.vel) * (1.0 / DT) - own
    } else {
        // Contact during the tick: reflect off a fixed ball, or trade
        // momentum along the normal with a free one, then find the thrust
        // that ends the tick at the bounced velocity.
        let (pa, va) = drift(a.pos, a.vel, a.thrust, a.inertia, toi * DT);
        let (pb, vb) = drift(b.pos, b.vel, b.thrust, b.inertia, toi * DT);
        let normal = (pb - pa).unit();
        let van = normal.dot(va);
        let after = if b.free {
            let vbn = normal.dot(vb);
            let dv = (2.0 * b.mass * vbn + (a.mass * van - b.mass * van)) / (b.mass + a.mass) - van;
            va + normal * dv
        } else {
            va - normal * (2.0 * van)
        };
        let (_, v_end) = drift(pa, after, a.thrust, a.inertia, (1.0 - toi) * DT);
        let k = (-FRICTION * DT / a.inertia).exp();
        (a.thrust * (a.inertia * (k - 1.0)) - a_prev_vel * (FRICTION * k) + v_end * FRICTION) * (-1.0 / a.inertia / (k - 1.0))
    };
    Some((toi, push))
}

/// The roots of a t^2 + b t + c, larger first (0x1000e120).
fn roots(a: f64, b: f64, c: f64) -> Option<(f64, f64)> {
    if a == 0.0 {
        return None;
    }
    let disc = b * b - a * c * 4.0;
    if disc < 0.0 {
        return None;
    }
    let s = disc.sqrt();
    Some(((s - b) * 0.5 / a, (-b - s) * 0.5 / a))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rifter() -> ShipStats {
        ShipStats::new(37.0, 1_100_000.0, 3.1, 320.0, 2.0)
    }

    fn pilot() -> Pilot {
        Pilot {
            type_id: 587,
            group_id: 25,
            name: "Rifter".into(),
            char_id: 140000001,
            corp_id: 1000002,
            alliance_id: None,
            security: 0.0,
            bounty: 0.0,
        }
    }

    fn park_with_ship(vel: Vec3) -> Park {
        let gate = Celestial {
            id: 50000001,
            type_id: 16,
            group_id: 10,
            name: "Gate".into(),
            pos: Vec3::new(100_000.0, 0.0, 0.0),
            radius: 2500.0,
            jump: None,
        };
        let mut park = Park::new(30002780, vec![gate], 0);
        park.ships.insert(1, Ship::new(1, 10, rifter(), pilot(), Vec3::ZERO, vel));
        park
    }

    #[test]
    fn a_stopped_ship_coasts_to_rest_after_undocking() {
        let mut park = park_with_ship(Vec3::new(320.0, 0.0, 0.0));
        park.advance(60);
        let ship = &park.ships[&1];
        assert!(ship.vel.len() < 0.01, "still moving at {}", ship.vel.len());
        // Coast distance is v0 * tau.
        let coast = 320.0 * rifter().tau();
        assert!((ship.pos.x - coast).abs() < 1.0, "coasted {} expected {coast}", ship.pos.x);
    }

    #[test]
    fn a_heading_reaches_cruise_speed_at_the_destiny_rate() {
        let mut park = park_with_ship(Vec3::ZERO);
        park.command(1, Command::Direction(Vec3::new(0.0, 0.0, 1.0)));
        park.set_speed_fraction(1, 0.5);
        let tau = rifter().tau();
        park.advance(tau.round() as i64);
        let v = park.ships[&1].vel.z;
        // After ~tau seconds the gap to 160 m/s has shrunk to ~1/e.
        assert!(v > 160.0 * 0.55 && v < 160.0 * 0.75, "v = {v}");
        park.advance(120);
        assert!((park.ships[&1].vel.z - 160.0).abs() < 0.1);
    }

    #[test]
    fn approaching_a_gate_stops_at_the_requested_range() {
        let mut park = park_with_ship(Vec3::ZERO);
        park.command(1, Command::Follow { target: 50000001, range: 0.0 });
        let eta = park.eta_within(1, 50000001, 2500.0, 3600).expect("arrives");
        assert!(eta > 200 && eta < 600, "eta {eta}s for ~97 km at 320 m/s");
        park.advance(1200);
        let gap = park.surface_distance(1, 50000001).unwrap();
        assert!(gap.abs() < 50.0, "settled {gap} m from the surface");
    }

    #[test]
    fn a_warp_aligns_then_lands_short_of_the_target() {
        let mut park = park_with_ship(Vec3::ZERO);
        // ~33 AU: several seconds in warp for a Rifter.
        let target = Vec3::new(5.0e12, 0.0, 0.0);
        park.command(1, Command::Warp { target, range: 15_000.0 });
        park.advance(4);
        assert!(!park.ships[&1].in_warp(), "still aligning after 4 s");
        park.advance(7);
        assert!(park.ships[&1].in_warp(), "aligned (75% speed after ln4 * tau = 4.7 s) and in warp");
        park.advance(60);
        let ship = &park.ships[&1];
        assert!(!ship.in_warp());
        assert_eq!(ship.command, Command::Stop);
        // It drops out with less than its top speed left and coasts on past
        // the exit point, at most (tau - 1) seconds of that speed.
        let short = (target - ship.pos).len();
        let coast = 320.0 * (rifter().tau() - 1.0);
        assert!(short <= 15_000.0 + 320.0 && short >= 15_000.0 - coast, "landed {short} m short");
    }

    #[test]
    fn a_warp_shorter_than_100_km_is_a_goto() {
        let mut park = park_with_ship(Vec3::ZERO);
        park.command(1, Command::Warp { target: Vec3::new(50_000.0, 0.0, 0.0), range: 0.0 });
        assert_eq!(park.ships[&1].command, Command::Point(Vec3::new(50_000.0, 0.0, 0.0)));
        assert!(!park.ships[&1].warping());
    }

    #[test]
    fn a_stopped_ship_levels_off() {
        let mut park = park_with_ship(Vec3::new(0.0, 100.0, 0.0));
        park.advance(1);
        let expect = 100.0 * (1.0 - STOP_LEVELLING) / STOP_LEVELLING_DIV * (-1.0 / rifter().tau()).exp();
        assert!((park.ships[&1].vel.y - expect).abs() < 1e-9, "vy {}", park.ships[&1].vel.y);
    }

    #[test]
    fn ships_on_a_collision_course_bounce_off_each_other() {
        let mut park = park_with_ship(Vec3::ZERO);
        park.ships.insert(2, Ship::new(2, 20, rifter(), pilot(), Vec3::new(4_000.0, 0.0, 0.0), Vec3::ZERO));
        park.command(1, Command::Direction(Vec3::new(1.0, 0.0, 0.0)));
        park.command(2, Command::Direction(Vec3::new(-1.0, 0.0, 0.0)));
        // The first contact turns both round before their hulls meet.
        while park.ships[&1].vel.x >= 0.0 {
            assert!(park.tick < 20, "never bounced");
            park.advance(park.tick + 1);
        }
        let (a, b) = (&park.ships[&1], &park.ships[&2]);
        assert!(b.pos.x - a.pos.x >= 2.0 * 37.0, "hulls overlap: {} m apart", b.pos.x - a.pos.x);
        assert!(b.vel.x > 0.0, "the other turned round too");
    }

    #[test]
    fn a_station_lets_ships_through_but_a_planet_does_not() {
        let at = |group_id: i64| Celestial {
            id: 40_000_001,
            type_id: 1,
            group_id,
            name: "Big".into(),
            pos: Vec3::ZERO,
            radius: 20_000.0,
            jump: None,
        };
        for (group, pushed) in [(GROUP_STATION, false), (7, true)] {
            let mut park = Park::new(30002780, vec![at(group)], 0);
            park.ships.insert(1, Ship::new(1, 10, rifter(), pilot(), Vec3::new(15_000.0, 0.0, 0.0), Vec3::ZERO));
            park.advance(5);
            let moved = park.ships[&1].pos.x - 15_000.0;
            assert_eq!(moved > 100.0, pushed, "group {group}: moved {moved} m");
        }
    }

    #[test]
    fn an_orbit_settles_near_the_orbit_radius() {
        let mut park = park_with_ship(Vec3::ZERO);
        park.ships.get_mut(&1).unwrap().pos = Vec3::new(100_000.0, 0.0, 10_000.0);
        park.command(1, Command::Orbit { target: 50000001, range: 5_000.0 });
        park.advance(600);
        let gap = park.surface_distance(1, 50000001).unwrap();
        assert!((gap - 5_000.0).abs() < 1_500.0, "orbiting {gap} m from the surface");
    }

    #[test]
    fn a_long_idle_park_catches_up_in_bounded_time() {
        let mut park = park_with_ship(Vec3::ZERO);
        park.advance(10_000_000);
        assert_eq!(park.tick, 10_000_000);
    }
}
