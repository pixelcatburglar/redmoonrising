//! Bot starbases defend themselves: shoot a bot alliance's tower and the
//! alliance answers. Its pilots already in the system answer at once: those
//! in space break off and warp to the tower, those docked undock. After
//! `RESPONSE` the rest of the fleet comes through a gate. They form up by
//! the tower and go for the attacker (and whoever attacks after them). Once
//! nobody has fired on the tower for `CALM`, the fleet goes home.

use std::time::{Duration, Instant};

use tokio_postgres::Client;

use super::battle::{self, Report};
use super::behaviour::{self, Atlas, Bot};
use super::politics::Politics;
use super::presence::{Identity, Presence, Rally};
use crate::ballpark::Vec3;
use crate::combat::Rng;
use crate::world::World;

/// How long the owners take to answer.
const RESPONSE: Duration = Duration::from_secs(20);
/// How long after the last shot at the tower the fleet stays.
const CALM: Duration = Duration::from_secs(3 * 60);
/// One ship for this many pilots in the owners' alliance, within bounds.
const PILOTS_PER_SHIP: u32 = 60;
const FLEET_MIN: usize = 4;
const FLEET_MAX: usize = 20;
/// How far outside the force field the fleet forms up.
const OUT: f64 = 10_000.0;

/// A tower's owners, as the bots know them.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Owners {
    Alliance(i32),
    Corporation(i32),
}

impl Owners {
    fn include(self, id: &Identity) -> bool {
        match self {
            Owners::Alliance(a) => id.alliance == Some(a),
            Owners::Corporation(c) => id.corp == c,
        }
    }
}

/// A tower being defended.
pub struct Defense {
    tower: i64,
    system: i32,
    owners: Owners,
    /// The ship the fleet goes for: the last to fire on the tower.
    attacker: i64,
    /// When the fleet arrives.
    due: Instant,
    last_alarm: Instant,
    rally: Rally,
    /// How many ships the owners mean to send.
    size: usize,
    fleet: Vec<u32>,
    /// Whether those from outside the system have been called.
    called_out: bool,
}

/// Answers the alarms raised since the last call, and sends home the fleets
/// whose towers have been left alone.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    defenses: &mut Vec<Defense>,
    world: &World,
    db: &Client,
    atlas: &Atlas,
    bots: &mut [Bot],
    ids: &[Identity],
    presence: &mut Presence,
    p: &Politics,
    rng: &mut Rng,
    clock: u32,
) -> Report {
    let mut report = Report::default();
    let now = Instant::now();

    // Alarms: (system, tower, attacker, tower's alliance and corporation, where).
    let alarms: Vec<(i32, i64, i64, Option<i64>, i64, Vec3, f64)> = {
        let mut w = world.lock();
        let mut found = Vec::new();
        for (system, park) in w.parks.iter_mut() {
            for (tower, attacker) in park.tower_alarms.drain(..) {
                if let Some(t) = park.structures.get(&tower) {
                    found.push((*system as i32, tower, attacker, t.alliance_id, t.corp_id, t.pos, t.field_radius.max(t.radius)));
                }
            }
        }
        found
    };
    for (system, tower, attacker, alliance, corp, at, field) in alarms {
        let owners = match alliance.map(|a| a as i32) {
            Some(a) if p.alliances.contains_key(&a) => Owners::Alliance(a),
            _ if p.corps.contains_key(&(corp as i32)) => Owners::Corporation(corp as i32),
            _ => continue,
        };
        if let Some(d) = defenses.iter_mut().find(|d| d.tower == tower) {
            d.last_alarm = now;
            if d.attacker != attacker {
                d.attacker = attacker;
                d.rally.target = Some(attacker);
                for i in &d.fleet {
                    presence.retarget(*i, attacker);
                }
            }
            continue;
        }
        let dir = rng.in_sphere(1.0);
        let dir = if dir.len() > 1e-6 { dir.unit() } else { Vec3::new(1.0, 0.0, 0.0) };
        let rally = Rally { anchor: tower, point: at + dir * (field + OUT), orbit: field + OUT, target: Some(attacker) };
        let who = match owners {
            Owners::Alliance(a) => super::sov::alliance_name(db, a).await,
            Owners::Corporation(c) => format!("corporation {c}"),
        };
        let place = super::sov::system_name(db, system).await;
        tracing::info!("bots: {who}'s tower in {place} is under attack; a fleet answers in {}s", RESPONSE.as_secs());
        report.news.push((crate::bulkdata::filetime_now(), format!("{who}'s tower in {place} is under attack: a fleet is on its way")));
        let pilots: u32 = match owners {
            Owners::Alliance(a) => p.corps.values().filter(|c| c.alliance == Some(a)).map(|c| c.members).sum(),
            Owners::Corporation(c) => p.corps.get(&c).map_or(0, |c| c.members),
        };
        let size = ((pilots / PILOTS_PER_SHIP) as usize).clamp(FLEET_MIN, FLEET_MAX).min(presence.cap() / 2);
        // Those already here answer at once.
        let fleet = battle::pick(bots, ids, presence, size, system, 1, |id| owners.include(id));
        for i in &fleet {
            battle::enlist(bots, presence, *i, system, rally, clock);
        }
        report.touched.extend(&fleet);
        tracing::info!("bots: {} ship(s) already in {place} answer at once", fleet.len());
        defenses.push(Defense { tower, system, owners, attacker, due: now + RESPONSE, last_alarm: now, rally, size, fleet, called_out: false });
    }

    let mut kept = Vec::new();
    for mut d in std::mem::take(defenses) {
        if d.last_alarm.elapsed() >= CALM {
            for i in d.fleet {
                presence.dismiss(i);
                let bot = &mut bots[i as usize];
                if bot.activity == behaviour::Activity::Sieging {
                    behaviour::release(bot, atlas, rng);
                    report.released.push(i);
                }
                report.touched.push(i);
            }
            continue;
        }
        if !d.called_out && now >= d.due {
            // The rest come from further off, through a gate.
            d.called_out = true;
            let owners = d.owners;
            let more = battle::pick(bots, ids, presence, d.size.saturating_sub(d.fleet.len()), d.system, 3, |id| owners.include(id));
            for i in &more {
                battle::enlist(bots, presence, *i, d.system, d.rally, clock);
            }
            tracing::info!("bots: {} more ship(s) come to defend tower {} in {}", more.len(), d.tower, d.system);
            report.touched.extend(&more);
            d.fleet.extend(more);
            if d.fleet.is_empty() {
                // Nobody to send: the alarm goes unanswered.
                continue;
            }
        }
        kept.push(d);
    }
    *defenses = kept;
    report
}
