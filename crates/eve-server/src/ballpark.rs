//! Server-side destiny: what every ship in a solar system is doing and where it is.
//!
//! The client runs the same simulation (destiny.dll) from the commands we send it,
//! so this only has to be close, not exact. It answers "where is that ship now":
//! for a resync (SetState), for the other players who see it, and for range
//! checks such as docking and jumping.
//!
//! Motion follows destiny's model: velocity converges exponentially on a target
//! velocity with time constant tau = mass * agility / 1e6 seconds, one tick per
//! second (const.simulationTimeStep = 1000 ms), integrated exactly over the tick.

use std::collections::BTreeMap;
use std::ops::{Add, Mul, Sub};

/// Ticks a park evolves at most per request; a park left alone longer than this
/// skips ahead instead (its ships have long since stopped).
const MAX_CATCH_UP: i64 = 3600;
/// A warp starts once the ship is at this fraction of its speed and aligned.
const WARP_ALIGN_SPEED: f64 = 0.75;
const WARP_ALIGN_COS: f64 = 0.98;
/// Speed of a warpSpeedMultiplier-1 ship, m/s (3 AU/s).
const BASE_WARP_SPEED: f64 = 3.0 * 149_597_870_700.0;

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
        (self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }
    pub fn dot(self, o: Vec3) -> f64 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }
    /// The unit vector, or zero for a (near) zero vector.
    pub fn unit(self) -> Vec3 {
        let l = self.len();
        if l < 1e-9 { Vec3::ZERO } else { self * (1.0 / l) }
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

/// What a ship's type makes it do.
#[derive(Debug, Clone, Copy)]
pub struct ShipStats {
    pub radius: f64,
    pub mass: f64,
    pub agility: f64,
    pub max_velocity: f64,
    /// m/s in warp.
    pub warp_speed: f64,
}

impl ShipStats {
    pub fn new(radius: f64, mass: f64, agility: f64, max_velocity: f64, warp_multiplier: f64) -> Self {
        ShipStats {
            radius,
            mass,
            agility,
            max_velocity,
            warp_speed: BASE_WARP_SPEED * if warp_multiplier > 0.0 { warp_multiplier } else { 1.0 },
        }
    }

    /// Seconds for the velocity to close 63% of the gap to its target.
    pub fn tau(&self) -> f64 {
        (self.mass * self.agility / 1e6).max(0.05)
    }
}

/// Who flies a ship, for its SlimItem.
#[derive(Debug, Clone)]
pub struct Pilot {
    pub type_id: i64,
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
    Aligning,
    InWarp { from: Vec3, to: Vec3, elapsed: f64, duration: f64 },
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
    warp: Option<WarpState>,
}

impl Ship {
    pub fn new(id: i64, client_id: i64, stats: ShipStats, pilot: Pilot, pos: Vec3, vel: Vec3) -> Self {
        Ship { id, client_id, stats, pilot, pos, vel, speed_fraction: 1.0, command: Command::Stop, warp: None }
    }

    pub fn in_warp(&self) -> bool {
        matches!(self.warp, Some(WarpState::InWarp { .. }))
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
}

impl Park {
    pub fn new(solar_system_id: i64, celestials: Vec<Celestial>, tick: i64) -> Self {
        Park { solar_system_id, tick, celestials, ships: BTreeMap::new() }
    }

    pub fn celestial(&self, id: i64) -> Option<&Celestial> {
        self.celestials.iter().find(|c| c.id == id)
    }

    /// Position and radius of any ball in the park.
    pub fn ball(&self, id: i64) -> Option<(Vec3, f64)> {
        if let Some(s) = self.ships.get(&id) {
            return Some((s.pos, s.stats.radius));
        }
        self.celestial(id).map(|c| (c.pos, c.radius))
    }

    /// Surface-to-surface distance between two balls.
    pub fn surface_distance(&self, a: i64, b: i64) -> Option<f64> {
        let (pa, ra) = self.ball(a)?;
        let (pb, rb) = self.ball(b)?;
        Some((pa - pb).len() - ra - rb)
    }

    /// Brings every ship forward to `tick`.
    pub fn advance(&mut self, tick: i64) {
        if tick <= self.tick {
            return;
        }
        let from = self.tick.max(tick - MAX_CATCH_UP);
        for _ in from..tick {
            self.step();
        }
        self.tick = tick;
    }

    /// Gives a ship a new command at the park's current tick.
    pub fn command(&mut self, ship_id: i64, command: Command) {
        if let Some(ship) = self.ships.get_mut(&ship_id) {
            ship.warp = matches!(command, Command::Warp { .. }).then_some(WarpState::Aligning);
            ship.command = command;
        }
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

    fn step(&mut self) {
        // Targets are read from the state at the start of the tick.
        let snapshot: Vec<(i64, Vec3, Vec3, f64)> =
            self.ships.values().map(|s| (s.id, s.pos, s.vel, s.stats.radius)).collect();
        let celestials = &self.celestials;
        let target_of = |id: i64| -> Option<(Vec3, Vec3, f64)> {
            if let Some((_, p, v, r)) = snapshot.iter().find(|(sid, ..)| *sid == id) {
                return Some((*p, *v, *r));
            }
            celestials.iter().find(|c| c.id == id).map(|c| (c.pos, Vec3::ZERO, c.radius))
        };
        for ship in self.ships.values_mut() {
            step_ship(ship, &target_of);
        }
    }
}

/// Advances one ship by one second.
fn step_ship(ship: &mut Ship, target_of: &dyn Fn(i64) -> Option<(Vec3, Vec3, f64)>) {
    const DT: f64 = 1.0;
    let tau = ship.stats.tau();
    let cruise = ship.stats.max_velocity * ship.speed_fraction;

    // In warp the ship is on rails: no inertia, straight to the exit point.
    if let Some(WarpState::InWarp { from, to, elapsed, duration }) = ship.warp {
        let elapsed = elapsed + DT;
        if elapsed >= duration {
            ship.pos = to;
            ship.vel = Vec3::ZERO;
            ship.warp = None;
            ship.command = Command::Stop;
        } else {
            ship.pos = from + (to - from) * (elapsed / duration);
            ship.warp = Some(WarpState::InWarp { from, to, elapsed, duration });
        }
        return;
    }

    // Approaching something: full cruise until close, then slow so as to arrive.
    // Aiming at gap / (4 tau) makes the approach critically damped: with the
    // velocity lagging its target by tau, anything faster overshoots.
    let approach = |pos: Vec3, gap: f64| -> Vec3 {
        if gap <= 0.0 {
            return Vec3::ZERO;
        }
        (pos - ship.pos).unit() * cruise.min(gap / (4.0 * tau))
    };

    let target_vel = match &ship.command {
        Command::Stop => Vec3::ZERO,
        Command::Direction(d) => d.unit() * cruise,
        Command::Point(p) => approach(*p, (*p - ship.pos).len()),
        Command::Follow { target, range } => match target_of(*target) {
            Some((tp, tv, tr)) => {
                let gap = (tp - ship.pos).len() - tr - ship.stats.radius - range;
                if gap > 0.0 { approach(tp, gap) } else { tv }
            }
            None => Vec3::ZERO,
        },
        Command::Orbit { target, range } => match target_of(*target) {
            Some((tp, _, tr)) => {
                let out = ship.pos - tp;
                let radius = range + tr + ship.stats.radius;
                // Keep the current sense of rotation; pick one if there is none.
                let mut tangent = ship.vel - out.unit() * ship.vel.dot(out.unit());
                if tangent.len() < 1e-6 {
                    tangent = Vec3::new(-out.z, 0.0, out.x);
                }
                let radial = out.unit() * ((radius - out.len()) / tau).clamp(-cruise, cruise);
                (tangent.unit() * cruise + radial).unit() * cruise
            }
            None => Vec3::ZERO,
        },
        Command::Warp { target, .. } => (*target - ship.pos).unit() * ship.stats.max_velocity,
    };

    // Exact integration of v' = (target - v) / tau over the tick.
    let k = (-DT / tau).exp();
    ship.pos = ship.pos + target_vel * DT + (ship.vel - target_vel) * (tau * (1.0 - k));
    ship.vel = target_vel + (ship.vel - target_vel) * k;

    // Aligned and fast enough: enter warp.
    if let (Some(WarpState::Aligning), Command::Warp { target, range }) = (ship.warp, &ship.command) {
        let to_target = *target - ship.pos;
        let speed = ship.vel.len();
        let aligned = speed > 0.0 && ship.vel.unit().dot(to_target.unit()) >= WARP_ALIGN_COS;
        if aligned && speed >= WARP_ALIGN_SPEED * ship.stats.max_velocity {
            let exit = *target - to_target.unit() * *range;
            let duration = ((exit - ship.pos).len() / ship.stats.warp_speed).max(1.0).ceil() + 2.0;
            ship.warp = Some(WarpState::InWarp { from: ship.pos, to: exit, elapsed: 0.0, duration });
        }
    }
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
        assert!(((target - ship.pos).len() - 15_000.0).abs() < 1.0);
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
