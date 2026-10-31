//! The observed half: where a player is in space, the bots in that system
//! become ships there, flying what the simulation says they are doing.
//! Miners and ratters orbit belts, pirates sit on gates, travellers warp
//! from gate to gate. A bot that jumps in comes out of the gate it would have
//! used, one that jumps out goes through the gate to its next system, and one
//! that undocks or docks does it at its station. When nobody is left to
//! watch, the system's bots go back to being numbers.
//!
//! At most `PER_SYSTEM` bots show in one system; the rest stay unseen. A bot
//! ship can be shot and destroyed, as a player's: its pilot ejects in a
//! capsule (`eject`) and makes for home, and the capsule can be destroyed
//! in turn, waking the pilot at home in station. A capsule that gets home
//! docks, and its pilot is in a ship again.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use tracing::info;

use super::behaviour::{Activity, Atlas, Bot};
use crate::ballpark::{Celestial, Command, Park, Pilot, Ship, ShipStats, Vec3};
use crate::combat::{attr, Rng};
use crate::npc::NpcType;
use crate::svc::space;
use crate::world::{World, WorldState};

/// The most bots shown in one system at once.
pub const PER_SYSTEM: usize = 120;
const GROUP_BELT: i64 = 9;
const GROUP_STARGATE: i64 = 10;
const ATTR_WARP_SPEED_MULTIPLIER: i32 = 600;
pub const TYPE_CAPSULE: i64 = 670;

/// The capsule's figures, for ejecting in the middle of a fight.
static CAPSULE: OnceLock<NpcType> = OnceLock::new();
/// How fast a bot flies when it is not in a hurry.
const CRUISE: f64 = 0.5;

/// What a bot is to anyone looking: its name, corporation, standing with the
/// law, and the ship it flies.
#[derive(Debug, Clone)]
pub struct Identity {
    pub name: String,
    pub corp: i32,
    pub security: f32,
    pub ship: i64,
    pub hull: i64,
    pub alliance: Option<i32>,
}

/// A step that a watcher would see.
#[derive(Debug, Clone, Copy)]
pub enum Change {
    Jumped { bot: u32, from: i32, to: i32 },
    Undocked { bot: u32, station: i32 },
    Docked { bot: u32 },
    /// Still in space, doing something else.
    Busy { bot: u32 },
}

pub struct Presence {
    /// The bots shown: in which system, and the ship they are in.
    shown: HashMap<u32, (i32, i64)>,
    hulls: HashMap<i64, NpcType>,
    /// Systems with a player in space, as of the last sync.
    observed: HashSet<i32>,
}

impl Presence {
    pub fn new(hulls: HashMap<i64, NpcType>) -> Presence {
        if let Some(capsule) = hulls.get(&TYPE_CAPSULE) {
            let _ = CAPSULE.set(capsule.clone());
        }
        Presence { shown: HashMap::new(), hulls, observed: HashSet::new() }
    }

    pub fn shown(&self) -> usize {
        self.shown.len()
    }

    pub fn observed(&self) -> usize {
        self.observed.len()
    }

    /// Systems with a player in space, as of the last sync.
    pub fn watched(&self) -> &HashSet<i32> {
        &self.observed
    }

    /// After a step: what changed that a watcher would see.
    pub fn note(&self, i: u32, before: (i32, i32, Activity), bot: &Bot, changes: &mut Vec<Change>) {
        let (system, station, activity) = before;
        let watched = |s: i32| self.observed.contains(&s);
        if station == 0 && bot.station == 0 && system != bot.system {
            if watched(system) || watched(bot.system) {
                changes.push(Change::Jumped { bot: i, from: system, to: bot.system });
            }
        } else if station != 0 && bot.station == 0 {
            if watched(bot.system) {
                changes.push(Change::Undocked { bot: i, station });
            }
        } else if station == 0 && bot.station != 0 {
            if watched(system) {
                changes.push(Change::Docked { bot: i });
            }
        } else if bot.station == 0 && activity != bot.activity && watched(bot.system) {
            changes.push(Change::Busy { bot: i });
        }
    }

    /// Brings space up to date with the bots: what they did since the last
    /// sync, played out, then every watched system holding the bots it
    /// should. Returns the bots whose ships or capsules were lost, with the
    /// seconds until each one's next step.
    #[allow(clippy::too_many_arguments)]
    pub fn sync(
        &mut self,
        world: &World,
        atlas: &Atlas,
        bots: &mut [Bot],
        ids: &[Identity],
        by_char: &HashMap<i64, u32>,
        changes: Vec<Change>,
        rng: &mut Rng,
    ) -> Vec<(u32, u32)> {
        let mut w = world.lock();
        let now = crate::combat::now_ms();
        let observed: HashSet<i32> = w.parks.iter().filter(|(_, p)| p.players().next().is_some()).map(|(id, _)| *id as i32).collect();

        // Lost ships leave their pilots in capsules, heading home; lost
        // capsules leave them at home.
        let mut lost = Vec::new();
        let losses: Vec<(i64, i64, Option<i64>)> =
            w.parks.iter_mut().flat_map(|(system, park)| park.bot_losses.drain(..).map(|(c, pod)| (*system, c, pod)).collect::<Vec<_>>()).collect();
        for (system, char_id, pod) in losses {
            let Some(&i) = by_char.get(&char_id) else { continue };
            let bot = &mut bots[i as usize];
            match pod {
                Some(capsule) => {
                    let wait = super::behaviour::podded(bot, capsule, atlas, rng);
                    self.shown.insert(i, (system as i32, capsule));
                    let command = w.parks.get(&system).and_then(|p| Some(command_for(bot, p, p.ships.get(&capsule)?.pos)));
                    if let Some(command) = command {
                        space::order(&mut w, system, capsule, command);
                    }
                    lost.push((i, wait));
                }
                None => {
                    self.shown.remove(&i);
                    lost.push((i, super::behaviour::reclone(bot, atlas, rng)));
                }
            }
        }

        for change in changes {
            match change {
                Change::Jumped { bot, from, to } => {
                    if self.shown.get(&bot).is_some_and(|(s, _)| *s == from) {
                        let gate = gate_to(&w, from, to);
                        self.hide(&mut w, bot, ids, gate.map(|g| (g, "effects.JumpOut")));
                    }
                    let b = &bots[bot as usize];
                    if observed.contains(&to) && b.station == 0 && b.system == to && self.room(to) {
                        let arrival = gate_to(&w, to, from).and_then(|g| w.parks.get(&(to as i64)).and_then(|p| p.celestial(g).cloned()));
                        let at = arrival.as_ref().map(|g| space::arrival_point(g.pos, g.radius));
                        self.show(&mut w, bot, b, &ids[bot as usize], at, arrival.map(|g| g.id), now);
                    }
                }
                Change::Undocked { bot, station } => {
                    let b = &bots[bot as usize];
                    if observed.contains(&b.system) && !self.shown.contains_key(&bot) && self.room(b.system) {
                        let at = w.parks.get(&(b.system as i64)).and_then(|p| p.celestial(station as i64)).map(|s| beside(s, b.id, 2_000.0));
                        self.show(&mut w, bot, b, &ids[bot as usize], at, None, now);
                    }
                }
                Change::Docked { bot, .. } => {
                    if self.shown.contains_key(&bot) {
                        self.hide(&mut w, bot, ids, None);
                    }
                }
                Change::Busy { bot } => {
                    if let Some(&(system, ship)) = self.shown.get(&bot) {
                        let b = &bots[bot as usize];
                        let command = w.parks.get(&(system as i64)).and_then(|p| Some(command_for(b, p, p.ships.get(&ship)?.pos)));
                        if let Some(command) = command {
                            space::order(&mut w, system as i64, ship, command);
                        }
                    }
                }
            }
        }

        // Out of sight: unwatched systems, and bots that are no longer there.
        let stale: Vec<u32> = self
            .shown
            .iter()
            .filter(|(i, (s, _))| {
                let b = &bots[**i as usize];
                !observed.contains(s) || b.system != *s || b.station != 0 || !b.online
            })
            .map(|(i, _)| *i)
            .collect();
        for i in stale {
            self.hide(&mut w, i, ids, None);
        }

        // Into sight: every watched system's bots in space, up to the cap.
        let mut wanted: HashMap<i32, Vec<u32>> = HashMap::new();
        for (i, b) in bots.iter().enumerate() {
            if b.online && b.station == 0 && observed.contains(&b.system) {
                wanted.entry(b.system).or_default().push(i as u32);
            }
        }
        for (system, list) in wanted {
            for i in list {
                if !self.room(system) {
                    break;
                }
                if !self.shown.contains_key(&i) {
                    self.show(&mut w, i, &bots[i as usize], &ids[i as usize], None, None, now);
                }
            }
        }

        let (before, after) = (self.observed.len(), observed.len());
        if before != after {
            info!("bots: {after} system(s) watched, {} bot ship(s) shown", self.shown.len());
        }
        self.observed = observed;
        lost
    }

    fn room(&self, system: i32) -> bool {
        self.shown.values().filter(|(s, _)| *s == system).count() < PER_SYSTEM
    }

    /// Puts a bot's ship in space: at `at`, or where its activity puts it;
    /// out of `gate` (with the jump-in) if it came through one.
    fn show(&mut self, w: &mut WorldState, i: u32, bot: &Bot, id: &Identity, at: Option<Vec3>, gate: Option<i64>, now: i64) {
        // A podded bot flies its capsule.
        let (ship_id, hull) = match bot.pod {
            Some(capsule) => (capsule, TYPE_CAPSULE),
            None => (id.ship, id.hull),
        };
        let Some(hull) = self.hulls.get(&hull) else { return };
        let system = bot.system as i64;
        let Some(park) = w.parks.get(&system) else { return };
        if park.ships.contains_key(&ship_id) {
            return;
        }
        let pos = at.unwrap_or_else(|| appear_at(bot, park));
        let command = command_for(bot, park, pos);
        let ship = ship_for(bot, id, ship_id, hull, pos, command, now);
        space::add_ship(w, system, ship);
        if let Some(gate) = gate {
            let tick = w.parks.get(&system).map_or(0, |p| p.tick);
            space::broadcast(w, system, None, vec![space::special_fx(tick, ship_id, Some(gate), "effects.JumpIn")]);
        }
        self.shown.insert(i, (bot.system, ship_id));
    }

    /// Takes a bot's ship out of space, through `effect` (a gate jump) if given.
    fn hide(&mut self, w: &mut WorldState, i: u32, _ids: &[Identity], effect: Option<(i64, &str)>) {
        let Some((system, ship)) = self.shown.remove(&i) else { return };
        if let Some((target, guid)) = effect {
            let tick = w.parks.get(&(system as i64)).map_or(0, |p| p.tick);
            space::broadcast(w, system as i64, None, vec![space::special_fx(tick, ship, Some(target), guid)]);
        }
        space::remove_ship(w, system as i64, ship);
    }
}

/// The gate in `from` that leads to `to`.
fn gate_to(w: &WorldState, from: i32, to: i32) -> Option<i64> {
    let park = w.parks.get(&(from as i64))?;
    park.celestials.iter().find(|c| c.group_id == GROUP_STARGATE && c.jump.is_some_and(|(s, _)| s == to as i64)).map(|c| c.id)
}

/// A point near a celestial, the same for the same bot.
fn beside(c: &Celestial, bot: i32, gap: f64) -> Vec3 {
    let dir = Rng::seeded(bot as u64).in_sphere(1.0);
    let dir = if dir.len() > 1e-6 { dir.unit() } else { Vec3::new(1.0, 0.0, 0.0) };
    c.pos + dir * (c.radius + gap)
}

/// One of a park's celestials of a group, chosen by the bot.
fn pick(park: &Park, group: i64, bot: i32) -> Option<&Celestial> {
    let all: Vec<&Celestial> = park.celestials.iter().filter(|c| c.group_id == group).collect();
    all.get(bot as usize % all.len().max(1)).copied()
}

/// Where a bot already in space is found when someone starts watching.
fn appear_at(bot: &Bot, park: &Park) -> Vec3 {
    let near = match bot.activity {
        Activity::Mining | Activity::Ratting => pick(park, GROUP_BELT, bot.id),
        Activity::Camping => pick(park, GROUP_STARGATE, bot.id),
        _ => park.celestials.get(bot.id as usize % park.celestials.len().max(1)),
    };
    near.map_or(Vec3::ZERO, |c| beside(c, bot.id, 10_000.0))
}

/// What a bot's ship does for what the bot is doing, from where it is.
fn command_for(bot: &Bot, park: &Park, from: Vec3) -> Command {
    let warp_to = |c: &Celestial, gap: f64| {
        let dir = (from - c.pos).unit();
        let dir = if dir.len() > 1e-6 { dir } else { Vec3::new(1.0, 0.0, 0.0) };
        Command::Warp { target: c.pos + dir * (c.radius + gap), range: 0.0 }
    };
    match bot.activity {
        Activity::Mining | Activity::Ratting => match pick(park, GROUP_BELT, bot.id) {
            Some(belt) if (belt.pos - from).len() > 200_000.0 => warp_to(belt, 5_000.0),
            Some(belt) => Command::Orbit { target: belt.id, range: 4_000.0 + (bot.id % 7) as f64 * 1_000.0 },
            None => Command::Stop,
        },
        Activity::Camping => match pick(park, GROUP_STARGATE, bot.id) {
            Some(gate) if (gate.pos - from).len() > 200_000.0 => warp_to(gate, 8_000.0),
            Some(gate) => Command::Orbit { target: gate.id, range: 6_000.0 + (bot.id % 5) as f64 * 2_000.0 },
            None => Command::Stop,
        },
        Activity::Travelling => {
            let next_gate = bot
                .route
                .last()
                .and_then(|next| park.celestials.iter().find(|c| c.group_id == GROUP_STARGATE && c.jump.is_some_and(|(s, _)| s == *next as i64)));
            let station = (bot.dock_at != 0).then(|| park.celestial(bot.dock_at as i64)).flatten();
            match next_gate.or(station) {
                Some(c) => warp_to(c, 1_500.0),
                None => Command::Stop,
            }
        }
        Activity::Docked => Command::Stop,
    }
}

/// A bot's ship blown up: its pilot's capsule, beside the wreck and still,
/// shown to the system. Returns the capsule's ID.
pub fn eject(park: &mut Park, ship: &Ship, now: i64, out: &mut crate::heartbeat::Out) -> Option<i64> {
    let hull = CAPSULE.get()?;
    let capsule = crate::npc::next_id();
    let id = Identity {
        name: ship.pilot.name.clone(),
        corp: ship.pilot.corp_id as i32,
        security: ship.pilot.security as f32,
        ship: capsule,
        hull: TYPE_CAPSULE,
        alliance: ship.pilot.alliance_id.map(|a| a as i32),
    };
    let bot = Bot {
        id: ship.pilot.char_id as i32,
        role: super::behaviour::Role::Miner,
        home: 0,
        system: park.solar_system_id as i32,
        station: 0,
        online: true,
        share: 0.0,
        session_end: 0,
        activity: Activity::Docked,
        route: Vec::new(),
        dock_at: 0,
        cargo: 0.0,
        isk: 0.0,
        pod: Some(capsule),
        turf: 0,
    };
    let pod = ship_for(&bot, &id, capsule, hull, ship.pos, Command::Stop, now);
    park.ships.insert(capsule, pod);
    for e in space::add_ball_entries(park, capsule) {
        out.destiny(park.solar_system_id, e);
    }
    Some(capsule)
}

fn ship_for(bot: &Bot, id: &Identity, ship_id: i64, hull: &NpcType, pos: Vec3, command: Command, now: i64) -> Ship {
    let a = |x: i32| hull.get(x);
    let top = a(attr::MAX_VELOCITY).max(50.0);
    let agility = if a(attr::AGILITY) > 0.0 { a(attr::AGILITY) } else { 1.0 };
    let mass = if hull.mass > 0.0 { hull.mass } else { 1_000_000.0 };
    let stats = ShipStats::new(hull.radius.max(20.0), mass, agility, top, a(ATTR_WARP_SPEED_MULTIPLIER));
    let pilot = Pilot {
        type_id: hull.type_id,
        group_id: hull.group_id,
        name: id.name.clone(),
        char_id: bot.id as i64,
        corp_id: id.corp as i64,
        alliance_id: id.alliance.map(i64::from),
        security: id.security as f64,
        bounty: 0.0,
    };
    let mut ship = Ship::new(ship_id, 0, stats, pilot, pos, Vec3::ZERO);
    ship.bot = true;
    ship.fight = Some(crate::npc::entity_fighter(hull, 30_000.0, 1, top, now));
    ship.speed_fraction = if matches!(command, Command::Warp { .. }) { 1.0 } else { CRUISE };
    ship.command = command;
    ship
}

/// The hull types bots fly, loaded once.
pub async fn hulls(db: &tokio_postgres::Client, ids: &[Identity]) -> HashMap<i64, NpcType> {
    let mut types: Vec<i64> = ids.iter().map(|i| i.hull).filter(|h| *h != 0).chain([TYPE_CAPSULE]).collect();
    types.sort_unstable();
    types.dedup();
    match crate::npc::load_types(db, &types).await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("bots: reading the hulls: {}", crate::logging::chain(&e));
            HashMap::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gates::Graph;

    const HERE: i32 = 30_000_001;
    const THERE: i32 = 30_000_002;

    fn celestial(id: i64, group_id: i64, pos: Vec3, jump: Option<(i64, i64)>) -> Celestial {
        Celestial { id, type_id: 0, group_id, name: String::new(), pos, radius: 1_000.0, jump }
    }

    /// A system with a belt and a gate to THERE, and a player in space.
    fn watched_world() -> World {
        let world = World::default();
        let celestials = vec![
            celestial(40_000_001, GROUP_BELT, Vec3::new(1.0e9, 0.0, 0.0), None),
            celestial(50_000_001, GROUP_STARGATE, Vec3::new(-1.0e9, 0.0, 0.0), Some((THERE as i64, 50_000_002))),
        ];
        let mut park = Park::new(HERE as i64, celestials, 0);
        let pilot = Pilot { type_id: 587, group_id: 25, name: "Player".into(), char_id: 90, corp_id: 1, alliance_id: None, security: 0.0, bounty: 0.0 };
        park.ships.insert(900, Ship::new(900, 77, ShipStats::new(30.0, 1e6, 1.0, 300.0, 1.0), pilot, Vec3::ZERO, Vec3::ZERO));
        world.lock().parks.insert(HERE as i64, park);
        world
    }

    fn atlas() -> Atlas {
        static GRAPH: std::sync::OnceLock<Graph> = std::sync::OnceLock::new();
        let graph = GRAPH.get_or_init(|| HashMap::from([(HERE as i64, vec![THERE as i64]), (THERE as i64, vec![HERE as i64])]));
        Atlas { systems: HashMap::new(), station_system: HashMap::from([(60_000_001, THERE)]), hubs: Vec::new(), belts_near: HashMap::new(), graph }
    }

    fn miner() -> (Bot, Identity) {
        let bot = Bot {
            id: 140_500_000, role: super::super::behaviour::Role::Miner, home: 60_000_001, system: HERE, station: 0, online: true,
            share: 0.3, session_end: u32::MAX, activity: Activity::Mining, route: Vec::new(), dock_at: 0, cargo: 0.0, isk: 0.0, pod: None, turf: 0,
        };
        let id = Identity { name: "Bot Miner".into(), corp: 1000044, security: 0.5, ship: 140_500_001, hull: 587, alliance: None };
        (bot, id)
    }

    fn presence() -> Presence {
        let hull = NpcType::with(587, 25, 30.0, 1e6, &[(attr::MAX_VELOCITY, 300.0), (attr::AGILITY, 3.0), (attr::HP, 300.0)]);
        let capsule = NpcType::with(TYPE_CAPSULE, 29, 25.0, 32_000.0, &[(attr::MAX_VELOCITY, 1_000.0), (attr::AGILITY, 1.0), (attr::HP, 200.0)]);
        Presence::new(HashMap::from([(587, hull), (TYPE_CAPSULE, capsule)]))
    }

    #[test]
    fn a_bot_in_a_watched_system_flies_there_and_leaves_through_its_gate() {
        let (world, atlas) = (watched_world(), atlas());
        let (bot, id) = miner();
        let (mut bots, ids) = (vec![bot], vec![id]);
        let by_char = HashMap::from([(bots[0].id as i64, 0)]);
        let mut p = presence();

        let mut rng = Rng::seeded(1);
        p.sync(&world, &atlas, &mut bots, &ids, &by_char, Vec::new(), &mut rng);
        {
            let w = world.lock();
            let ship = &w.parks[&(HERE as i64)].ships[&ids[0].ship];
            assert!(ship.bot && !ship.is_player() && ship.is_pilot());
            assert!(matches!(ship.command, Command::Orbit { target: 40_000_001, .. } | Command::Warp { .. }));
        }
        assert_eq!(p.shown(), 1);

        // It jumps to THERE, which nobody watches: out through the gate.
        bots[0].system = THERE;
        p.sync(&world, &atlas, &mut bots, &ids, &by_char, vec![Change::Jumped { bot: 0, from: HERE, to: THERE }], &mut rng);
        assert!(!world.lock().parks[&(HERE as i64)].ships.contains_key(&ids[0].ship));
        assert_eq!(p.shown(), 0);
    }

    #[test]
    fn a_destroyed_ship_leaves_a_capsule_and_a_destroyed_capsule_a_clone_at_home() {
        let (world, atlas) = (watched_world(), atlas());
        let (bot, id) = miner();
        let (mut bots, ids) = (vec![bot], vec![id]);
        let by_char = HashMap::from([(bots[0].id as i64, 0)]);
        let mut p = presence();
        let mut rng = Rng::seeded(1);
        p.sync(&world, &atlas, &mut bots, &ids, &by_char, Vec::new(), &mut rng);

        // The ship dies: the capsule is out, and heads for home through the gate.
        let capsule = {
            let mut w = world.lock();
            let park = w.parks.get_mut(&(HERE as i64)).unwrap();
            let ship = park.ships.remove(&ids[0].ship).unwrap();
            let pod = eject(park, &ship, 0, &mut crate::heartbeat::Out::default()).unwrap();
            park.bot_losses.push((bots[0].id as i64, Some(pod)));
            pod
        };
        let lost = p.sync(&world, &atlas, &mut bots, &ids, &by_char, Vec::new(), &mut rng);
        assert_eq!(lost.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0]);
        assert_eq!((bots[0].pod, bots[0].activity), (Some(capsule), Activity::Travelling));
        {
            let w = world.lock();
            let pod = &w.parks[&(HERE as i64)].ships[&capsule];
            assert_eq!(pod.pilot.type_id, TYPE_CAPSULE);
            assert!(matches!(pod.command, Command::Warp { .. }), "{:?}", pod.command);
        }

        // The capsule dies: the pilot wakes at home.
        {
            let mut w = world.lock();
            let park = w.parks.get_mut(&(HERE as i64)).unwrap();
            park.ships.remove(&capsule);
            park.bot_losses.push((bots[0].id as i64, None));
        }
        p.sync(&world, &atlas, &mut bots, &ids, &by_char, Vec::new(), &mut rng);
        assert_eq!((bots[0].station, bots[0].system, bots[0].pod), (60_000_001, THERE, None));

        // The player leaves: a bot back in space here is not shown.
        world.lock().parks.get_mut(&(HERE as i64)).unwrap().ships.remove(&900);
        bots[0] = miner().0;
        p.sync(&world, &atlas, &mut bots, &ids, &by_char, Vec::new(), &mut rng);
        assert_eq!(p.shown(), 0);
    }
}
