//! Sieges fought where someone can see them.
//!
//! When a siege comes due (`run`) where no player is in space, it is settled
//! by the odds (`sov::conclude`). Where one is, both alliances send a fleet:
//! the defenders form up outside their tower's force field, the attackers
//! come through a gate and warp in further out. They fight with the guns and
//! brains bot ships have (their war makes them enemies on sight), and the
//! ships lost are lost for real, killmails and all. When one fleet is gone
//! the other has won; attackers who clear the field bombard the tower for
//! `BOMBARD`, and it falls. A fight still going after `MAX_FIGHT` goes to
//! whoever has more ships left, and one nobody watches any more to the odds
//! of what is left. Then the survivors go home.

use std::time::{Duration, Instant};

use tokio_postgres::Client;
use tracing::info;

use super::behaviour::{self, Activity, Atlas, Bot};
use super::politics::Politics;
use super::presence::{Identity, Presence, Rally};
use super::sov::{self, News, Siege};
use crate::combat::Rng;
use crate::world::World;

/// A fleet is one ship for this many pilots in its alliance, within bounds.
const PILOTS_PER_SHIP: u32 = 30;
const FLEET_MIN: usize = 6;
const FLEET_MAX: usize = 40;
/// Room left in the system for bots that are not in either fleet.
const ROOM_FOR_OTHERS: usize = 10;
/// Where the fleets form up, beyond the tower's force field.
const DEFENDERS_OUT: f64 = 15_000.0;
const ATTACKERS_IN: f64 = 55_000.0;
const ATTACKERS_ORBIT: f64 = 35_000.0;
const MAX_FIGHT: Duration = Duration::from_secs(15 * 60);
const BOMBARD: Duration = Duration::from_secs(60);

/// The two fleets of a siege being fought, as indexes into the bots.
pub struct Fight {
    pub attackers: Vec<u32>,
    pub defenders: Vec<u32>,
    began: Instant,
    /// When the last defender fell, and the bombardment began.
    cleared: Option<Instant>,
}

impl Fight {
    /// Ships still fighting on each side.
    pub fn standing(&self, bots: &[Bot], system: i32) -> (usize, usize) {
        let up = |list: &[u32]| {
            list.iter()
                .filter(|i| {
                    let b = &bots[**i as usize];
                    b.activity == Activity::Sieging && b.pod.is_none() && b.system == system
                })
                .count()
        };
        (up(&self.attackers), up(&self.defenders))
    }
}

/// What a pass over the sieges (or the defences) did.
#[derive(Default)]
pub struct Report {
    pub news: Vec<News>,
    /// Bots called up or sent home, whose state changed.
    pub touched: Vec<u32>,
    /// Bots sent home, whose ships should set off.
    pub released: Vec<u32>,
}

impl Report {
    pub fn merge(&mut self, other: Report) {
        self.news.extend(other.news);
        self.touched.extend(other.touched);
        self.released.extend(other.released);
    }
}

/// Starts the sieges that are due, and follows the ones being fought.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    sieges: &mut Vec<Siege>,
    world: &World,
    db: &Client,
    atlas: &Atlas,
    bots: &mut [Bot],
    ids: &[Identity],
    presence: &mut Presence,
    p: &mut Politics,
    rng: &mut Rng,
    clock: u32,
    claim_delay_secs: i64,
    speed: f64,
) -> Result<Report, tokio_postgres::Error> {
    let mut report = Report::default();
    let mut kept = Vec::new();
    for mut s in std::mem::take(sieges) {
        let watched = presence.watched().contains(&s.system);
        let verdict = match &mut s.fight {
            None if clock < s.starts => None,
            None => {
                let fight = if watched { muster(&s, world, bots, ids, presence, rng, clock) } else { None };
                match fight {
                    Some(f) => {
                        info!("bots: the siege of {} begins: {} ships against {}", s.names.system, f.attackers.len(), f.defenders.len());
                        let now = crate::bulkdata::filetime_now();
                        report.news.push((now, format!("Battle in {}: {} ships of {} engage {} of {}", s.names.system, f.attackers.len(), s.names.attacker, f.defenders.len(), s.names.defender)));
                        report.touched.extend(f.attackers.iter().chain(&f.defenders));
                        s.fight = Some(f);
                        None
                    }
                    // Nobody to see it (or nothing there to fight over): the odds.
                    None => {
                        let won = rng.unit() < s.attackers as f64 / (s.attackers + s.defenders).max(1) as f64;
                        let lost = if won { (1 + rng.below(4), 3 + rng.below(8)) } else { (3 + rng.below(8), 1 + rng.below(4)) };
                        Some((won, (lost.0 as u32, lost.1 as u32)))
                    }
                }
            }
            Some(f) => {
                let (a, d) = f.standing(bots, s.system);
                let lost = ((f.attackers.len() - a) as u32, (f.defenders.len() - d) as u32);
                if !watched {
                    Some((rng.unit() < a as f64 / (a + d).max(1) as f64, lost))
                } else if a == 0 {
                    Some((false, lost))
                } else if d == 0 {
                    let since = *f.cleared.get_or_insert_with(Instant::now);
                    (since.elapsed() >= BOMBARD).then_some((true, lost))
                } else if f.began.elapsed() >= MAX_FIGHT {
                    Some((a > d, lost))
                } else {
                    None
                }
            }
        };
        let Some((won, lost)) = verdict else {
            kept.push(s);
            continue;
        };
        report.news.push(sov::conclude(db, p, &s, won, lost, rng, claim_delay_secs, speed).await?);
        sov::show_towers(db, world, s.system).await;
        if let Some(f) = s.fight.take() {
            for i in f.attackers.into_iter().chain(f.defenders) {
                presence.dismiss(i);
                let bot = &mut bots[i as usize];
                if bot.activity == Activity::Sieging {
                    behaviour::release(bot, atlas, rng);
                    report.released.push(i);
                }
                report.touched.push(i);
            }
        }
    }
    *sieges = kept;
    Ok(report)
}

/// How near a bot is to answer a call to `system`: 0 in space there, 1
/// docked there, 2 playing elsewhere, 3 not playing.
pub fn nearness(b: &Bot, system: i32) -> u8 {
    match (b.online, b.system == system, b.station == 0) {
        (true, true, true) => 0,
        (true, true, false) => 1,
        (true, false, _) => 2,
        (false, _, _) => 3,
    }
}

/// Up to `n` bots for a fleet to `system`: armed, in their ships, not in
/// another fleet, no further off than `furthest` (`nearness`), and on the
/// side `member` says; the nearest first.
#[allow(clippy::too_many_arguments)]
pub fn pick(bots: &[Bot], ids: &[Identity], presence: &Presence, n: usize, system: i32, furthest: u8, member: impl Fn(&Identity) -> bool) -> Vec<u32> {
    let mut fit: Vec<u32> = (0..bots.len() as u32)
        .filter(|i| {
            let (b, id) = (&bots[*i as usize], &ids[*i as usize]);
            member(id) && b.pod.is_none() && b.activity != Activity::Sieging && presence.armed(id.hull) && nearness(b, system) <= furthest
        })
        .collect();
    fit.sort_by_key(|i| nearness(&bots[*i as usize], system));
    fit.truncate(n);
    fit
}

/// Calls a bot up to a fleet in `system`: it comes out of its station if
/// it is docked there.
pub fn enlist(bots: &mut [Bot], presence: &mut Presence, i: u32, system: i32, rally: Rally, clock: u32) {
    let b = &mut bots[i as usize];
    let docked_here = (b.online && b.system == system && b.station != 0).then_some(b.station);
    behaviour::muster(b, system, clock);
    presence.muster(i, rally, docked_here);
}

/// Calls up both fleets for a siege in a watched system, around the
/// defender's tower there. None if the defender has no tower left there.
#[allow(clippy::too_many_arguments)]
fn muster(s: &Siege, world: &World, bots: &mut [Bot], ids: &[Identity], presence: &mut Presence, rng: &mut Rng, clock: u32) -> Option<Fight> {
    let (tower, at, field) = {
        let w = world.lock();
        let park = w.parks.get(&(s.system as i64))?;
        let t = park.structures.values().find(|t| t.is_tower() && t.alliance_id == Some(s.defender as i64))?;
        (t.id, t.pos, t.field_radius.max(t.radius))
    };
    let dir = rng.in_sphere(1.0);
    let dir = if dir.len() > 1e-6 { dir.unit() } else { crate::ballpark::Vec3::new(1.0, 0.0, 0.0) };
    let room = presence.cap().saturating_sub(ROOM_FOR_OTHERS) / 2;
    let size = |pilots: u32| ((pilots / PILOTS_PER_SHIP) as usize).clamp(FLEET_MIN, FLEET_MAX).min(room.max(1));
    let attackers = pick(bots, ids, presence, size(s.attackers), s.system, 3, |id| id.alliance == Some(s.attacker));
    let defenders = pick(bots, ids, presence, size(s.defenders), s.system, 3, |id| id.alliance == Some(s.defender));
    if attackers.is_empty() || defenders.is_empty() {
        return None;
    }
    let defend = Rally { anchor: tower, point: at + dir * (field + DEFENDERS_OUT), orbit: field + DEFENDERS_OUT, target: None };
    let attack = Rally { anchor: tower, point: at + dir * (field + ATTACKERS_IN), orbit: field + ATTACKERS_ORBIT, target: None };
    for (list, rally) in [(&attackers, attack), (&defenders, defend)] {
        for i in list {
            enlist(bots, presence, *i, s.system, rally, clock);
        }
    }
    Some(Fight { attackers, defenders, began: Instant::now(), cleared: None })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::ballpark::{Celestial, Park, Pilot, Ship, ShipStats, Vec3};
    use crate::combat::attr;
    use crate::npc::NpcType;
    use crate::pos::Structure;

    const HERE: i32 = 30_000_001;
    const ATTACKER: i32 = 99_000_001;
    const DEFENDER: i32 = 99_000_002;
    const TOWER: i64 = 1_000_000_001;

    fn tower() -> Structure {
        Structure {
            id: TOWER,
            type_id: 12235,
            group_id: 365,
            name: String::new(),
            owner_id: 98_000_002,
            corp_id: 98_000_002,
            alliance_id: Some(DEFENDER as i64),
            pos: Vec3::ZERO,
            radius: 3_000.0,
            state: crate::pos::state::ONLINE,
            stamp: None,
            tower: None,
            delays: [0; 3],
            vitals: crate::pos::vitals(None, 0),
            signature: 0.0,
            last_attacker: None,
            reinforce_asked: false,
            field_radius: 30_000.0,
            harmonic: -1,
            allow_corp: true,
            allow_alliance: true,
            field: None,
            sentry: Default::default(),
            guard: None,
        }
    }

    /// A system with a tower, a gate near it, and a player watching.
    fn world() -> World {
        let world = World::default();
        let gate = Celestial { id: 50_000_001, type_id: 0, group_id: 10, name: String::new(), pos: Vec3::new(60_000.0, 0.0, 0.0), radius: 1_000.0, jump: Some((30_000_002, 50_000_002)) };
        let mut park = Park::new(HERE as i64, vec![gate], 0);
        park.structures.insert(TOWER, tower());
        let pilot = Pilot { type_id: 587, group_id: 25, name: "Player".into(), char_id: 90, corp_id: 1, alliance_id: None, security: 0.0, bounty: 0.0 };
        park.ships.insert(900, Ship::new(900, 77, ShipStats::new(30.0, 1e6, 1.0, 300.0, 1.0), pilot, Vec3::new(0.0, 1.0e7, 0.0), Vec3::ZERO));
        world.lock().parks.insert(HERE as i64, park);
        world
    }

    fn frigate() -> NpcType {
        NpcType::with(
            587,
            25,
            30.0,
            1.0e6,
            &[
                (attr::MAX_VELOCITY, 350.0),
                (attr::AGILITY, 3.0),
                (attr::HP, 300.0),
                (attr::ARMOR_HP, 300.0),
                (attr::SHIELD_CAPACITY, 300.0),
                (attr::SCAN_RESOLUTION, 600.0),
                (attr::SIGNATURE_RADIUS, 35.0),
            ],
        )
    }

    #[test]
    fn a_watched_siege_is_fought_ship_against_ship() {
        crate::svc::war::in_force_for_test(ATTACKER as i64, DEFENDER as i64);
        let world = world();
        let capsule = NpcType::with(super::super::presence::TYPE_CAPSULE, 29, 25.0, 32_000.0, &[(attr::MAX_VELOCITY, 1_000.0), (attr::AGILITY, 1.0), (attr::HP, 200.0)]);
        let mut presence = Presence::new(HashMap::from([(587, frigate()), (super::super::presence::TYPE_CAPSULE, capsule)]), 120);
        let (mut bots, mut ids) = (Vec::new(), Vec::new());
        for n in 0..12u32 {
            let side = if n < 6 { ATTACKER } else { DEFENDER };
            bots.push(Bot {
                id: 140_600_000 + n as i32, role: behaviour::Role::Ratter, home: 60_000_001, system: 30_000_009, station: 60_000_001, online: true,
                share: 0.3, session_end: u32::MAX, activity: Activity::Docked, route: Vec::new(), dock_at: 0, cargo: 0.0, isk: 0.0, pod: None, turf: 0,
            });
            ids.push(Identity { name: format!("Pilot {n}"), corp: side - 1_000_000, security: 0.0, ship: 140_700_000 + n as i64, hull: 587, alliance: Some(side) });
        }
        let by_char: HashMap<i64, u32> = bots.iter().enumerate().map(|(i, b)| (b.id as i64, i as u32)).collect();
        static GRAPH: std::sync::OnceLock<crate::gates::Graph> = std::sync::OnceLock::new();
        let atlas = Atlas { systems: HashMap::new(), station_system: HashMap::from([(60_000_001, 30_000_009)]), hubs: Vec::new(), belts_near: HashMap::new(), graph: GRAPH.get_or_init(HashMap::new) };
        let mut rng = Rng::seeded(3);

        // Somebody is watching, so the fleets are called up.
        presence.sync(&world, &atlas, &mut bots, &ids, &by_char, Vec::new(), &mut rng);
        let names = sov::SiegeNames { attacker: "A".into(), defender: "D".into(), system: "Here".into(), region: "R".into(), security: -0.3 };
        let siege = Siege { war: 1, attacker: ATTACKER, defender: DEFENDER, attacker_exec: 1, attacker_race: 1, attackers: 300, defenders: 300, system: HERE, names, starts: 0, starts_at: 0, fight: None };
        let fight = muster(&siege, &world, &mut bots, &ids, &mut presence, &mut rng, 0).expect("both sides have armed ships");
        assert_eq!((fight.attackers.len(), fight.defenders.len()), (6, 6));
        presence.sync(&world, &atlas, &mut bots, &ids, &by_char, Vec::new(), &mut rng);
        assert_eq!(presence.shown(), 12, "both fleets are in space");

        // They fight: ships are lost on both sides' account.
        let mut now = crate::combat::now_ms();
        let mut lost = 0;
        for _ in 0..600 {
            now += 1_000;
            let mut w = world.lock();
            let park = w.parks.get_mut(&(HERE as i64)).unwrap();
            let tick = park.tick + 1;
            park.advance(tick);
            crate::fight::step(park, now, &mut crate::heartbeat::Out::default());
            lost += park.bot_losses.iter().filter(|(_, pod)| pod.is_some()).count();
            drop(w);
            presence.sync(&world, &atlas, &mut bots, &ids, &by_char, Vec::new(), &mut rng);
            let (a, d) = fight.standing(&bots, HERE);
            if a == 0 || d == 0 {
                break;
            }
        }
        let (a, d) = fight.standing(&bots, HERE);
        assert!(lost >= 6, "ships were destroyed: {lost}, standing {a} and {d}");
        assert!(a == 0 || d == 0, "one side was wiped out: {a} and {d}");
    }
}
