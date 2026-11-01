//! What a bot does, unobserved: a small state machine per role, stepped
//! whenever the bot's next moment comes up. A step changes where the bot is
//! and what it holds, and says how long until its next one. Nothing here
//! touches the database or the world; the runtime keeps the books.
//!
//! Bots play more at their prime time than at other hours (`activity`): the
//! hours their alliance (or, outside one, they themselves) favour, a curve
//! over the day whose mean is the bot's usual share of it. A region can be
//! made busier or quieter than usual (`Ctx::density`).

use std::collections::HashMap;

use super::economy::Economy;
use super::ships::{self, Shipyard};
use crate::combat::Rng;
use crate::gates::Graph;

/// What a bot is for. The mix and the places each lives follow
/// `population::ROLES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// Flying as a player's hired wingman (`hire`): wherever they go.
    Escorting,
}

impl Activity {
    pub fn name(self) -> &'static str {
        match self {
            Activity::Docked => "docked",
            Activity::Travelling => "travelling",
            Activity::Mining => "mining",
            Activity::Ratting => "ratting",
            Activity::Camping => "camping a gate",
            Activity::Sieging => "in a fleet",
            Activity::Escorting => "flying as a wingman",
        }
    }
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
    /// What is in the hold: ore mined, or goods hauled; (type, units).
    pub hold: Vec<(i32, i64)>,
    pub isk: f64,
    /// The wallet as last saved: what changed since is written as a change,
    /// so payments made to it meanwhile (by players) are kept.
    pub isk_saved: f64,
    /// Earned since the runtime last looked, for its corporation's tax.
    pub earned: f64,
    /// The capsule it is in, its ship lost, until it docks and takes a new one.
    pub pod: Option<i64>,
    /// Where it mines or rats: its alliance's space; 0 for near home.
    pub turf: i32,
    /// Its ship's type, and its rung on its role's ladder (`ships`).
    pub hull: i32,
    pub tier: u8,
    /// Its ship was lost: it buys another when next docked.
    pub hull_lost: bool,
    /// The hour (UTC) it most likes to play.
    pub prime: u8,
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
    pub fn system_of(&self, station: i32) -> i32 {
        self.station_system.get(&station).copied().unwrap_or(0)
    }

    pub fn region_of(&self, system: i32) -> i32 {
        self.systems.get(&system).map_or(0, |s| s.region)
    }

    /// The jumps from one system to another, next last; None if unreachable.
    pub fn route(&self, from: i32, to: i32) -> Option<Vec<i32>> {
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

/// What a step may look at and change beyond the bot itself.
pub struct Ctx<'a> {
    pub atlas: &'a Atlas,
    pub econ: &'a mut Economy,
    pub yard: Option<&'a Shipyard>,
    /// Seconds into the UTC day when the bots' clock read 0.
    pub day0: u32,
    /// How much more (or less) than usual the bots of a region play.
    pub density: &'a HashMap<i32, f32>,
}

/// A stretch of time in seconds, between `lo` and `hi`.
fn span(rng: &mut Rng, lo: u32, hi: u32) -> u32 {
    lo + rng.below((hi - lo + 1) as usize) as u32
}

/// How long one jump takes: align, warp across, jump.
fn jump_time(rng: &mut Rng) -> u32 {
    span(rng, 45, 120)
}

/// How busy a bot is at an hour of the UTC day against its average: most
/// at its prime time, least twelve hours off; the day's mean is 1.
pub fn activity(hour: f64, prime: u8) -> f64 {
    let phase = (hour - prime as f64) / 24.0 * std::f64::consts::TAU;
    1.0 + 0.75 * phase.cos()
}

/// The prime-time hours bots keep: Europe, the US, Australia and Russia
/// (UTC). Alliances keep one each (`politics`); others pick by their ID.
pub const PRIME_TIMES: [u8; 4] = [19, 2, 10, 16];

pub fn prime_for(id: i32) -> u8 {
    // Europe is the busiest, then the US.
    match id.rem_euclid(10) {
        0..=4 => PRIME_TIMES[0],
        5..=7 => PRIME_TIMES[1],
        8 => PRIME_TIMES[2],
        _ => PRIME_TIMES[3],
    }
}

impl Ctx<'_> {
    fn hour(&self, now: u32) -> f64 {
        ((self.day0 as u64 + now as u64) % 86_400) as f64 / 3_600.0
    }

    /// The share of the day a bot plays, at this hour, in its region.
    fn share_now(&self, bot: &Bot, now: u32) -> f64 {
        let region = self.atlas.region_of(self.atlas.system_of(bot.home));
        let density = self.density.get(&region).copied().unwrap_or(1.0) as f64;
        (bot.share as f64 * density * activity(self.hour(now), bot.prime)).clamp(0.01, 0.97)
    }
}

/// Where a bot starts the day: online for part of a session or offline,
/// as its share (at this hour) says.
pub fn wake_up(bot: &mut Bot, now: u32, ctx: &Ctx, rng: &mut Rng) -> u32 {
    bot.activity = Activity::Docked;
    bot.route.clear();
    let share = ctx.share_now(bot, now);
    if rng.unit() < share {
        bot.online = true;
        bot.session_end = now + span(rng, 600, 4 * 3_600);
        span(rng, 1, 300)
    } else {
        bot.online = false;
        let off = offline_time(share, rng);
        span(rng, 1, off)
    }
}

fn session_time(rng: &mut Rng) -> u32 {
    span(rng, 30 * 60, 4 * 3_600)
}

/// Off long enough that, over a day, the bot plays its share.
fn offline_time(share: f64, rng: &mut Rng) -> u32 {
    let share = share.clamp(0.02, 0.95);
    let mean = (2.0 * 3_600.0) * (1.0 / share - 1.0);
    (mean * rng.range(0.5, 1.5)) as u32
}

/// Its ship lost, a bot in its capsule makes for home; returns the seconds
/// until it sets off.
pub fn podded(bot: &mut Bot, capsule: i64, atlas: &Atlas, rng: &mut Rng) -> u32 {
    bot.pod = Some(capsule);
    bot.hold.clear();
    bot.hull_lost = true;
    go(bot, atlas, atlas.system_of(bot.home), bot.home, rng)
}

/// Its capsule (or, unseen, its ship) lost, a bot wakes at home in station,
/// to buy a new ship.
pub fn reclone(bot: &mut Bot, atlas: &Atlas, rng: &mut Rng) -> u32 {
    bot.pod = None;
    bot.station = bot.home;
    bot.system = atlas.system_of(bot.home);
    bot.activity = Activity::Docked;
    bot.route.clear();
    bot.dock_at = 0;
    bot.hold.clear();
    bot.hull_lost = true;
    span(rng, 300, 1_800)
}

/// Money a bot made: in its wallet, and counted for its corporation's tax.
fn earn(bot: &mut Bot, amount: f64) {
    bot.isk += amount;
    bot.earned += amount;
}

/// One step of a bot's life; returns the seconds until its next.
pub fn step(bot: &mut Bot, now: u32, ctx: &mut Ctx, rng: &mut Rng) -> u32 {
    let atlas = ctx.atlas;
    if !bot.online {
        bot.online = true;
        bot.session_end = now + session_time(rng);
        bot.activity = Activity::Docked;
        return span(rng, 30, 300);
    }
    // Time to stop: home and dock, then log off.
    if now >= bot.session_end && !matches!(bot.activity, Activity::Travelling | Activity::Sieging | Activity::Escorting) {
        if bot.station != 0 {
            bot.online = false;
            return offline_time(ctx.share_now(bot, now), rng);
        }
        return go(bot, atlas, atlas.system_of(bot.home), bot.home, rng);
    }
    match bot.activity {
        Activity::Travelling => travel(bot, atlas, rng),
        Activity::Docked => leave(bot, ctx, rng),
        Activity::Mining => {
            let m3 = mining_yield(bot, ctx.yard, rng);
            let security = atlas.systems.get(&bot.system).map_or(0.5, |s| s.security);
            match ctx.econ.ore_for(security, rng.unit()) {
                Some(ore) => {
                    let units = (m3 / ctx.econ.volume(ore)) as i64;
                    match bot.hold.iter_mut().find(|(t, _)| *t == ore) {
                        Some(h) => h.1 += units,
                        None => bot.hold.push((ore, units)),
                    }
                }
                // No market to sell into: the ore is as good as money.
                None => earn(bot, m3 * 20.0),
            }
            go(bot, atlas, atlas.system_of(bot.home), bot.home, rng)
        }
        Activity::Ratting => {
            let class = ctx.yard.map_or(0, |y| ships::weight_class(y.group_of(bot.hull))) as f64;
            let bounty = span(rng, 200_000, 1_500_000) as f64 * (1.0 + class * 1.5);
            earn(bot, bounty);
            go(bot, atlas, atlas.system_of(bot.home), bot.home, rng)
        }
        Activity::Camping => roam(bot, atlas, rng),
        // The fleet decides when it is over (`release`); the player, when
        // the hire is (`hire`).
        Activity::Sieging | Activity::Escorting => 60,
    }
}

/// What one trip to the belts brings in, m3: more for barges.
fn mining_yield(bot: &Bot, yard: Option<&Shipyard>, rng: &mut Rng) -> f64 {
    let (lo, hi) = match yard.map(|y| y.group_of(bot.hull)) {
        Some(ships::GROUP_EXHUMER) => (14_000, 26_000),
        Some(ships::GROUP_BARGE) => (8_000, 16_000),
        _ => (2_000, 6_000),
    };
    span(rng, lo, hi) as f64
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

/// The siege (or the hire) over: home, to dock; returns the seconds until
/// it sets off.
pub fn release(bot: &mut Bot, atlas: &Atlas, rng: &mut Rng) -> u32 {
    go(bot, atlas, atlas.system_of(bot.home), bot.home, rng)
}

/// Docked: buys a ship if it lost one, sells what it brought, then what the
/// role does next.
fn leave(bot: &mut Bot, ctx: &mut Ctx, rng: &mut Rng) -> u32 {
    let atlas = ctx.atlas;
    refit(bot, ctx, rng);
    // Home with ore: it is sold, and a while spent in station.
    if !bot.hold.is_empty() && bot.role != Role::Hauler {
        let earned = ctx.econ.sell_ore(bot.station, &mut bot.hold);
        earn(bot, earned);
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
        Role::Hauler => haul(bot, ctx, rng),
        Role::Trader => {
            // Mostly in station; now and then to another hub, which becomes
            // home, unless it keeps its hub's orders.
            if rng.unit() < 0.1 && !ctx.econ.is_trader(bot.id) && !atlas.hubs.is_empty() {
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

/// A hauler in station: hands over its load at a hub, takes on a load
/// where there is stock, or goes where there is some.
fn haul(bot: &mut Bot, ctx: &mut Ctx, rng: &mut Rng) -> u32 {
    let atlas = ctx.atlas;
    let econ = &mut *ctx.econ;
    if !bot.hold.is_empty() {
        if econ.is_hub(bot.station) {
            let pay = econ.unload(bot.station, &mut bot.hold);
            earn(bot, pay);
            return span(rng, 300, 1_200);
        }
        if let Some(hub) = econ.hub_for(bot.station) {
            return go(bot, atlas, atlas.system_of(hub), hub, rng);
        }
        bot.hold.clear();
    }
    let capacity = ctx.yard.map_or(3_000.0, |y| y.capacity(bot.hull)).max(500.0);
    let load = econ.load(bot.station, capacity);
    if !load.is_empty() {
        bot.hold = load;
        if let Some(hub) = econ.hub_for(bot.station) {
            return go(bot, atlas, atlas.system_of(hub), hub, rng);
        }
    }
    if let Some(source) = econ.source_near(atlas, bot.system, 6).filter(|s| *s != bot.station) {
        return go(bot, atlas, atlas.system_of(source), source, rng);
    }
    // Nothing to carry: a run to a hub and back, or to somewhere nearby.
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

/// A new ship: the one it lost again, or the best it can afford; or, now
/// and then, the next class up when its wallet has grown enough.
fn refit(bot: &mut Bot, ctx: &mut Ctx, rng: &mut Rng) {
    let Some(yard) = ctx.yard else {
        bot.hull_lost = false;
        return;
    };
    let race = yard.race_of(bot.hull);
    let seed = bot.id as u64;
    if bot.hull_lost {
        bot.hull_lost = false;
        for tier in (0..=bot.tier).rev() {
            let Some(hull) = yard.hull_for(bot.role, race, tier, seed) else { continue };
            // The cheapest rung is always to be had, money or not.
            let budget = if tier == 0 { f64::MAX } else { bot.isk };
            if let Some(price) = ctx.econ.buy_hull(hull, bot.station, budget, yard.price(hull)) {
                bot.isk -= price.min(bot.isk.max(0.0));
                bot.hull = hull;
                bot.tier = tier;
                return;
            }
        }
        return;
    }
    if rng.unit() >= 0.05 {
        return;
    }
    // The next rung that has a hull (a race may lack a class).
    let rungs = ships::ladder(bot.role).len() as u8;
    let Some((next, hull)) = (bot.tier + 1..rungs).find_map(|t| yard.hull_for(bot.role, race, t, seed).filter(|h| yard.tier_of(bot.role, *h) == t).map(|h| (t, h))) else {
        return;
    };
    if bot.isk < yard.price(hull) * ships::CUSHION {
        return;
    }
    if let Some(price) = ctx.econ.buy_hull(hull, bot.station, bot.isk, yard.price(hull)) {
        bot.isk -= price;
        bot.hull = hull;
        bot.tier = next;
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
pub fn go(bot: &mut Bot, atlas: &Atlas, system: i32, dock_at: i32, rng: &mut Rng) -> u32 {
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
pub mod tests {
    use super::*;

    pub fn atlas() -> Atlas {
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

    pub fn bot(role: Role) -> Bot {
        Bot {
            id: 1, role, home: 101, system: 1, station: 101, online: true, share: 0.5, session_end: u32::MAX,
            activity: Activity::Docked, route: Vec::new(), dock_at: 0, hold: Vec::new(), isk: 0.0, isk_saved: 0.0, earned: 0.0,
            pod: None, turf: 0, hull: 587, tier: 0, hull_lost: false, prime: 19,
        }
    }

    /// Steps a bot with an economy and no shipyard.
    fn run(b: &mut Bot, atlas: &Atlas, econ: &mut Economy, now: u32, rng: &mut Rng) -> u32 {
        let density = HashMap::new();
        let mut ctx = Ctx { atlas, econ, yard: None, day0: 0, density: &density };
        step(b, now, &mut ctx, rng)
    }

    #[test]
    fn a_miner_flies_to_the_belts_mines_and_comes_home_to_sell() {
        let (atlas, mut rng, mut b) = (atlas(), Rng::seeded(3), bot(Role::Miner));
        let mut econ = super::super::economy::tests::economy();
        run(&mut b, &atlas, &mut econ, 0, &mut rng);
        assert_eq!((b.activity, b.station), (Activity::Travelling, 0));
        run(&mut b, &atlas, &mut econ, 0, &mut rng);
        assert_eq!(b.system, 2);
        run(&mut b, &atlas, &mut econ, 0, &mut rng);
        assert_eq!(b.activity, Activity::Mining);
        run(&mut b, &atlas, &mut econ, 0, &mut rng);
        assert!(b.hold.iter().any(|(t, n)| *t == 1230 && *n > 0), "{:?}", b.hold);
        for _ in 0..3 {
            run(&mut b, &atlas, &mut econ, 0, &mut rng);
        }
        assert_eq!((b.system, b.station, b.activity), (1, 101, Activity::Docked));
        run(&mut b, &atlas, &mut econ, 0, &mut rng);
        assert!(b.isk > 0.0 && b.hold.is_empty() && b.earned == b.isk);
    }

    #[test]
    fn a_session_ends_docked_at_home() {
        let (atlas, mut rng, mut b) = (atlas(), Rng::seeded(5), bot(Role::Ratter));
        let mut econ = Economy::empty();
        b.station = 0;
        b.system = 2;
        b.activity = Activity::Ratting;
        b.session_end = 10;
        // Home, docked, then logged off.
        for _ in 0..4 {
            run(&mut b, &atlas, &mut econ, 20, &mut rng);
        }
        assert_eq!((b.station, b.online), (101, false));
    }

    #[test]
    fn a_podded_bot_flies_home_and_is_in_a_ship_once_docked() {
        let (atlas, mut rng, mut b) = (atlas(), Rng::seeded(2), bot(Role::Miner));
        let mut econ = Economy::empty();
        b.station = 0;
        b.system = 3;
        b.activity = Activity::Mining;
        podded(&mut b, 77, &atlas, &mut rng);
        assert_eq!((b.pod, b.activity, b.dock_at, b.hull_lost), (Some(77), Activity::Travelling, 101, true));
        for _ in 0..3 {
            run(&mut b, &atlas, &mut econ, 0, &mut rng);
        }
        assert_eq!((b.system, b.station, b.pod), (1, 101, None));
        b.pod = Some(78);
        reclone(&mut b, &atlas, &mut rng);
        assert_eq!((b.station, b.pod, b.activity), (101, None, Activity::Docked));
    }

    #[test]
    fn a_hauler_runs_to_a_hub_and_back() {
        let (atlas, mut rng, mut b) = (atlas(), Rng::seeded(11), bot(Role::Hauler));
        let mut econ = Economy::empty();
        let mut docked_at = Vec::new();
        for _ in 0..12 {
            run(&mut b, &atlas, &mut econ, 0, &mut rng);
            if b.activity == Activity::Docked && docked_at.last() != Some(&b.station) {
                docked_at.push(b.station);
            }
        }
        assert!(docked_at.contains(&301) && docked_at.contains(&101), "{docked_at:?}");
    }

    #[test]
    fn a_hauler_carries_minerals_from_where_they_were_made_to_the_hub() {
        let (atlas, mut rng, mut b) = (atlas(), Rng::seeded(4), bot(Role::Hauler));
        let mut econ = super::super::economy::tests::economy();
        let mut ore = vec![(1230, 333 * 100)];
        econ.sell_ore(101, &mut ore);
        econ.refine_all();
        run(&mut b, &atlas, &mut econ, 0, &mut rng);
        assert!(b.hold.iter().any(|(t, _)| *t == 34), "{:?}", b.hold);
        assert_eq!(b.dock_at, 301);
        for _ in 0..4 {
            run(&mut b, &atlas, &mut econ, 0, &mut rng);
        }
        assert_eq!(b.station, 301);
        let before = econ.ask(301, 34);
        run(&mut b, &atlas, &mut econ, 0, &mut rng);
        assert!(b.hold.is_empty() && b.isk > 0.0);
        assert!(econ.ask(301, 34) <= before);
    }

    #[test]
    fn bots_play_most_at_their_prime_time() {
        assert!((activity(19.0, 19) - 1.75).abs() < 1e-9);
        assert!((activity(7.0, 19) - 0.25).abs() < 1e-9);
        let mean: f64 = (0..240).map(|h| activity(h as f64 / 10.0, 2)).sum::<f64>() / 240.0;
        assert!((mean - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_bot_that_lost_its_ship_buys_another_or_the_best_it_can_afford() {
        let (atlas, mut rng) = (atlas(), Rng::seeded(1));
        let yard = super::super::ships::tests::yard();
        let mut econ = super::super::economy::tests::economy();
        let density = HashMap::new();
        let mut b = bot(Role::Ratter);
        b.hull = 621;
        b.tier = 2;
        b.hull_lost = true;
        b.isk = 1_000_000.0;
        let mut ctx = Ctx { atlas: &atlas, econ: &mut econ, yard: Some(&yard), day0: 0, density: &density };
        refit(&mut b, &mut ctx, &mut rng);
        // A cruiser costs more than it has: a frigate.
        assert_eq!((b.hull, b.tier, b.hull_lost), (587, 0, false));
        b.isk = 1e9;
        for _ in 0..200 {
            refit(&mut b, &mut ctx, &mut rng);
        }
        assert!(b.tier >= 2, "{}", b.tier);
    }
}
