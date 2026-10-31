//! What a bot does, unobserved: a small state machine per role, stepped
//! whenever the bot's next moment comes up. A step changes where the bot is
//! and what it holds, and says how long until its next one. Nothing here
//! touches the database or the world; the runtime keeps the books.

use std::collections::HashMap;

use crate::combat::Rng;
use crate::gates::Graph;

/// What a bot is for. The mix and the places each lives follow
/// `population::ROLES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Role {
    Miner = 1,
    Hauler = 2,
    Trader = 3,
    Ratter = 4,
    Explorer = 5,
    Pirate = 6,
}

impl Role {
    pub fn from_i16(v: i16) -> Role {
        match v {
            2 => Role::Hauler,
            3 => Role::Trader,
            4 => Role::Ratter,
            5 => Role::Explorer,
            6 => Role::Pirate,
            _ => Role::Miner,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Role::Miner => "miner",
            Role::Hauler => "hauler",
            Role::Trader => "trader",
            Role::Ratter => "ratter",
            Role::Explorer => "explorer",
            Role::Pirate => "pirate",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// In a station.
    Docked,
    /// Jumping along `route`; on arriving, docks at `dock_at` if set.
    Travelling,
    /// In a belt.
    Mining,
    Ratting,
    /// Sitting on a gate, waiting for someone to come through.
    Camping,
    /// In a siege fleet, at its alliance's call: stays until released.
    Sieging,
}

#[derive(Debug, Clone)]
pub struct Bot {
    pub id: i32,
    pub role: Role,
    /// The station it lives at and goes back to.
    pub home: i32,
    pub system: i32,
    /// The station it is docked in; 0 in space.
    pub station: i32,
    pub online: bool,
    /// The share of the day it plays.
    pub share: f32,
    pub session_end: u32,
    pub activity: Activity,
    /// The systems still to jump through, next last.
    pub route: Vec<i32>,
    pub dock_at: i32,
    /// Ore in the hold, in m3.
    pub cargo: f32,
    pub isk: f64,
    /// The capsule it is in, its ship lost, until it docks and takes a new one.
    pub pod: Option<i64>,
    /// Where it mines or rats: its alliance's space; 0 for near home.
    pub turf: i32,
}

/// The map as bots see it.
pub struct Atlas {
    pub systems: HashMap<i32, System>,
    pub station_system: HashMap<i32, i32>,
    /// The big trade hubs' stations.
    pub hubs: Vec<i32>,
    /// Each system's nearest system with belts, within a few jumps.
    pub belts_near: HashMap<i32, i32>,
    pub graph: &'static Graph,
}

pub struct System {
    pub security: f32,
    pub region: i32,
    pub stations: Vec<i32>,
    pub belts: u16,
    pub neighbours: Vec<i32>,
}

impl Atlas {
    fn system_of(&self, station: i32) -> i32 {
        self.station_system.get(&station).copied().unwrap_or(0)
    }

    /// The jumps from one system to another, next last; None if unreachable.
    fn route(&self, from: i32, to: i32) -> Option<Vec<i32>> {
        let mut path: Vec<i32> = crate::gates::route(self.graph, from as i64, to as i64)?.into_iter().map(|s| s as i32).collect();
        path.reverse();
        Some(path)
    }

    fn random_station_within(&self, from: i32, jumps: i64, rng: &mut Rng, keep: impl Fn(&System) -> bool) -> Option<i32> {
        let near: Vec<i32> = crate::gates::reach_on(self.graph, from as i64, jumps)
            .into_iter()
            .filter(|(_, n)| *n > 0)
            .filter_map(|(s, _)| self.systems.get(&(s as i32)).filter(|sys| !sys.stations.is_empty() && keep(sys)).map(|_| s as i32))
            .collect();
        let system = *near.get(rng.below(near.len().max(1)))?;
        let stations = &self.systems[&system].stations;
        stations.get(rng.below(stations.len())).copied()
    }
}

/// A stretch of time in seconds, between `lo` and `hi`.
fn span(rng: &mut Rng, lo: u32, hi: u32) -> u32 {
    lo + rng.below((hi - lo + 1) as usize) as u32
}

/// How long one jump takes: align, warp across, jump.
fn jump_time(rng: &mut Rng) -> u32 {
    span(rng, 45, 120)
}

/// Where a bot starts the day: online for part of a session or offline,
/// as its share says.
pub fn wake_up(bot: &mut Bot, now: u32, rng: &mut Rng) -> u32 {
    bot.activity = Activity::Docked;
    bot.route.clear();
    if rng.unit() < bot.share as f64 {
        bot.online = true;
        bot.session_end = now + span(rng, 600, 4 * 3_600);
        span(rng, 1, 300)
    } else {
        bot.online = false;
        let off = offline_time(bot, rng);
        span(rng, 1, off)
    }
}

fn session_time(rng: &mut Rng) -> u32 {
    span(rng, 30 * 60, 4 * 3_600)
}

/// Off long enough that, over a day, the bot plays its share.
fn offline_time(bot: &Bot, rng: &mut Rng) -> u32 {
    let share = bot.share.clamp(0.02, 0.95) as f64;
    let mean = (2.0 * 3_600.0) * (1.0 / share - 1.0);
    (mean * rng.range(0.5, 1.5)) as u32
}

/// Its ship lost, a bot in its capsule makes for home; returns the seconds
/// until it sets off.
pub fn podded(bot: &mut Bot, capsule: i64, atlas: &Atlas, rng: &mut Rng) -> u32 {
    bot.pod = Some(capsule);
    bot.cargo = 0.0;
    go(bot, atlas, atlas.system_of(bot.home), bot.home, rng)
}

/// Its capsule lost, a bot wakes at home in station.
pub fn reclone(bot: &mut Bot, atlas: &Atlas, rng: &mut Rng) -> u32 {
    bot.pod = None;
    bot.station = bot.home;
    bot.system = atlas.system_of(bot.home);
    bot.activity = Activity::Docked;
    bot.route.clear();
    bot.dock_at = 0;
    bot.cargo = 0.0;
    span(rng, 300, 1_800)
}

/// One step of a bot's life; returns the seconds until its next.
pub fn step(bot: &mut Bot, now: u32, atlas: &Atlas, rng: &mut Rng) -> u32 {
    if !bot.online {
        bot.online = true;
        bot.session_end = now + session_time(rng);
        bot.activity = Activity::Docked;
        return span(rng, 30, 300);
    }
    // Time to stop: home and dock, then log off.
    if now >= bot.session_end && !matches!(bot.activity, Activity::Travelling | Activity::Sieging) {
        if bot.station != 0 {
            bot.online = false;
            return offline_time(bot, rng);
        }
        return go(bot, atlas, atlas.system_of(bot.home), bot.home, rng);
    }
    match bot.activity {
        Activity::Travelling => travel(bot, atlas, rng),
        Activity::Docked => leave(bot, atlas, rng),
        Activity::Mining => {
            bot.cargo += span(rng, 2_000, 12_000) as f32;
            go(bot, atlas, atlas.system_of(bot.home), bot.home, rng)
        }
        Activity::Ratting => {
            bot.isk += span(rng, 200_000, 4_000_000) as f64;
            go(bot, atlas, atlas.system_of(bot.home), bot.home, rng)
        }
        Activity::Camping => roam(bot, atlas, rng),
        // The fleet decides when it is over (`release`).
        Activity::Sieging => 60,
    }
}

/// Called up for a siege: online, undocked and in `system`, where it stays
/// until `release`d.
pub fn muster(bot: &mut Bot, system: i32, now: u32) {
    if !bot.online {
        bot.online = true;
    }
    bot.session_end = bot.session_end.max(now + 2 * 3_600);
    bot.system = system;
    bot.station = 0;
    bot.route.clear();
    bot.dock_at = 0;
    bot.activity = Activity::Sieging;
}

/// The siege over: home, to dock; returns the seconds until it sets off.
pub fn release(bot: &mut Bot, atlas: &Atlas, rng: &mut Rng) -> u32 {
    go(bot, atlas, atlas.system_of(bot.home), bot.home, rng)
}

/// Docked: what the role does next.
fn leave(bot: &mut Bot, atlas: &Atlas, rng: &mut Rng) -> u32 {
    // Back home with ore: it is sold, and a while spent in station.
    if bot.cargo > 0.0 {
        bot.isk += bot.cargo as f64 * 20.0;
        bot.cargo = 0.0;
        return span(rng, 120, 900);
    }
    match bot.role {
        Role::Miner => {
            let belts = work_place(bot, atlas);
            work_at(bot, atlas, belts, Activity::Mining, rng)
        }
        Role::Ratter => {
            let belts = work_place(bot, atlas);
            work_at(bot, atlas, belts, Activity::Ratting, rng)
        }
        Role::Hauler => {
            // A run to a hub and back, or to somewhere nearby.
            let target = if bot.station == bot.home {
                let hub = atlas.hubs.get(rng.below(atlas.hubs.len().max(1))).copied();
                hub.filter(|_| rng.unit() < 0.5)
                    .or_else(|| atlas.random_station_within(bot.system, 8, rng, |s| s.security >= 0.45))
                    .unwrap_or(bot.home)
            } else {
                bot.home
            };
            go(bot, atlas, atlas.system_of(target), target, rng)
        }
        Role::Trader => {
            // Mostly in station; now and then to another hub, which becomes home.
            if rng.unit() < 0.1 {
                let hub = atlas.hubs[rng.below(atlas.hubs.len())];
                bot.home = hub;
                return go(bot, atlas, atlas.system_of(hub), hub, rng);
            }
            span(rng, 30 * 60, 2 * 3_600)
        }
        Role::Explorer => {
            let target = atlas.random_station_within(bot.system, 15, rng, |_| true).unwrap_or(bot.home);
            if rng.unit() < 0.3 {
                bot.home = target;
            }
            go(bot, atlas, atlas.system_of(target), 0, rng)
        }
        Role::Pirate => {
            bot.station = 0;
            bot.activity = Activity::Camping;
            span(rng, 10 * 60, 40 * 60)
        }
    }
}

/// Where a miner or ratter works: its turf, or the belts nearest to it.
fn work_place(bot: &Bot, atlas: &Atlas) -> i32 {
    if bot.turf != 0 {
        return bot.turf;
    }
    atlas.belts_near.get(&bot.system).copied().unwrap_or(bot.system)
}

/// Into space and on to `system`'s belts.
fn work_at(bot: &mut Bot, atlas: &Atlas, system: i32, work: Activity, rng: &mut Rng) -> u32 {
    if system == bot.system {
        bot.station = 0;
        bot.activity = work;
        return span(rng, 20 * 60, 60 * 60);
    }
    // Arriving in space with no station to dock at starts the work (`travel`).
    go(bot, atlas, system, 0, rng)
}

/// Sets off for `system`, to dock at `dock_at` (0: stay in space).
fn go(bot: &mut Bot, atlas: &Atlas, system: i32, dock_at: i32, rng: &mut Rng) -> u32 {
    bot.station = 0;
    bot.dock_at = dock_at;
    match atlas.route(bot.system, system) {
        Some(route) => {
            bot.route = route;
            bot.activity = Activity::Travelling;
            span(rng, 20, 60)
        }
        None => {
            // Cut off: it stays where it is.
            bot.activity = Activity::Docked;
            bot.station = atlas.systems.get(&bot.system).and_then(|s| s.stations.first().copied()).unwrap_or(bot.home);
            span(rng, 300, 900)
        }
    }
}

fn travel(bot: &mut Bot, atlas: &Atlas, rng: &mut Rng) -> u32 {
    if let Some(next) = bot.route.pop() {
        bot.system = next;
        return jump_time(rng);
    }
    if bot.dock_at != 0 {
        bot.station = bot.dock_at;
        bot.dock_at = 0;
        bot.activity = Activity::Docked;
        // Docked, a podded pilot is in a ship again.
        bot.pod = None;
        return span(rng, 120, 900);
    }
    // Arrived in space: the role's work here.
    match bot.role {
        Role::Miner => work_at(bot, atlas, bot.system, Activity::Mining, rng),
        Role::Ratter => work_at(bot, atlas, bot.system, Activity::Ratting, rng),
        Role::Pirate => {
            bot.activity = Activity::Camping;
            span(rng, 10 * 60, 40 * 60)
        }
        _ => {
            // Explorers and the rest look around a while, then find a station.
            let station = atlas.systems.get(&bot.system).and_then(|s| s.stations.first().copied());
            match station {
                Some(station) => {
                    bot.dock_at = station;
                    span(rng, 5 * 60, 30 * 60)
                }
                None => go(bot, atlas, atlas.system_of(bot.home), bot.home, rng),
            }
        }
    }
}

/// A pirate moves on: to a neighbouring system of the same kind of space,
/// sometimes home to dock.
fn roam(bot: &mut Bot, atlas: &Atlas, rng: &mut Rng) -> u32 {
    if rng.unit() < 0.15 {
        return go(bot, atlas, atlas.system_of(bot.home), bot.home, rng);
    }
    let here = atlas.systems.get(&bot.system);
    let options: Vec<i32> = here
        .map(|s| {
            s.neighbours
                .iter()
                .copied()
                .filter(|n| atlas.systems.get(n).is_some_and(|ns| ns.security < 0.45))
                .collect()
        })
        .unwrap_or_default();
    match options.get(rng.below(options.len().max(1))) {
        Some(&next) => {
            bot.route = vec![next];
            bot.dock_at = 0;
            bot.activity = Activity::Travelling;
            span(rng, 30, 120)
        }
        None => span(rng, 10 * 60, 30 * 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atlas() -> Atlas {
        // 1 - 2 - 3, a station in 1 and 3, belts in 2.
        static GRAPH: std::sync::OnceLock<Graph> = std::sync::OnceLock::new();
        let graph = GRAPH.get_or_init(|| HashMap::from([(1, vec![2]), (2, vec![1, 3]), (3, vec![2])]));
        let system = |stations: Vec<i32>, belts: u16, neighbours: Vec<i32>| System { security: 0.9, region: 1, stations, belts, neighbours };
        Atlas {
            systems: HashMap::from([(1, system(vec![101], 0, vec![2])), (2, system(vec![], 3, vec![1, 3])), (3, system(vec![301], 0, vec![2]))]),
            station_system: HashMap::from([(101, 1), (301, 3)]),
            hubs: vec![301],
            belts_near: HashMap::from([(1, 2), (2, 2), (3, 2)]),
            graph,
        }
    }

    fn bot(role: Role) -> Bot {
        Bot {
            id: 1, role, home: 101, system: 1, station: 101, online: true, share: 0.5, session_end: u32::MAX,
            activity: Activity::Docked, route: Vec::new(), dock_at: 0, cargo: 0.0, isk: 0.0, pod: None, turf: 0,
        }
    }

    #[test]
    fn a_miner_flies_to_the_belts_mines_and_comes_home_to_sell() {
        let (atlas, mut rng, mut b) = (atlas(), Rng::seeded(3), bot(Role::Miner));
        step(&mut b, 0, &atlas, &mut rng);
        assert_eq!((b.activity, b.station), (Activity::Travelling, 0));
        step(&mut b, 0, &atlas, &mut rng);
        assert_eq!(b.system, 2);
        step(&mut b, 0, &atlas, &mut rng);
        assert_eq!(b.activity, Activity::Mining);
        step(&mut b, 0, &atlas, &mut rng);
        assert!(b.cargo > 0.0);
        for _ in 0..3 {
            step(&mut b, 0, &atlas, &mut rng);
        }
        assert_eq!((b.system, b.station, b.activity), (1, 101, Activity::Docked));
        step(&mut b, 0, &atlas, &mut rng);
        assert!(b.isk > 0.0 && b.cargo == 0.0);
    }

    #[test]
    fn a_session_ends_docked_at_home() {
        let (atlas, mut rng, mut b) = (atlas(), Rng::seeded(5), bot(Role::Ratter));
        b.station = 0;
        b.system = 2;
        b.activity = Activity::Ratting;
        b.session_end = 10;
        // Home, docked, then logged off.
        for _ in 0..4 {
            step(&mut b, 20, &atlas, &mut rng);
        }
        assert_eq!((b.station, b.online), (101, false));
    }

    #[test]
    fn a_podded_bot_flies_home_and_is_in_a_ship_once_docked() {
        let (atlas, mut rng, mut b) = (atlas(), Rng::seeded(2), bot(Role::Miner));
        b.station = 0;
        b.system = 3;
        b.activity = Activity::Mining;
        podded(&mut b, 77, &atlas, &mut rng);
        assert_eq!((b.pod, b.activity, b.dock_at), (Some(77), Activity::Travelling, 101));
        for _ in 0..3 {
            step(&mut b, 0, &atlas, &mut rng);
        }
        assert_eq!((b.system, b.station, b.pod), (1, 101, None));
        b.pod = Some(78);
        reclone(&mut b, &atlas, &mut rng);
        assert_eq!((b.station, b.pod, b.activity), (101, None, Activity::Docked));
    }

    #[test]
    fn a_hauler_runs_to_a_hub_and_back() {
        let (atlas, mut rng, mut b) = (atlas(), Rng::seeded(11), bot(Role::Hauler));
        let mut docked_at = Vec::new();
        for _ in 0..12 {
            step(&mut b, 0, &atlas, &mut rng);
            if b.activity == Activity::Docked && docked_at.last() != Some(&b.station) {
                docked_at.push(b.station);
            }
        }
        assert!(docked_at.contains(&301) && docked_at.contains(&101), "{docked_at:?}");
    }
}
