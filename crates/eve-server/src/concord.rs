//! Crime and punishment: aggression between players, and CONCORD.
//!
//! Every offensive act of one pilot (or their drone) against another pilot's
//! ship is aggression, remembered for 15 minutes and told to the system as
//! OnAggressionChange (michelle.py:75), which is how the victim's client knows
//! it may shoot back without a warning. In high-security space (0.5 and up)
//! aggression against someone who is not fair game is a crime: the criminal's
//! security status drops and, after a delay that is shorter the higher the
//! security, CONCORD arrives and destroys the criminal's ship. Low-security
//! space costs status but brings no police.
//!
//! Theft is aggression too: taking from a can that belongs to another pilot
//! (consider.py:222) makes the thief fair game to the owner, and to the
//! owner's corporation if it is a player one, for 15 minutes. It is not a
//! crime: it costs no status and brings no police.
//!
//! Fair game: anyone who aggressed you or your corporation, anyone flagged a
//! criminal, and outlaws (security status -5 or lower).

use std::collections::BTreeMap;

use eve_marshal::Value;
use tracing::info;

use crate::ballpark::{Command, Park, Ship};
use crate::combat::{self, Lock};
use crate::heartbeat::{Job, Out};
use crate::services::{Call, Ctx, Outcome};
use crate::svc::space::{self, entry};
use crate::world::World;

const AGGRESSION_MS: i64 = 15 * 60_000;
/// Systems at or above this (as shown, rounded) are high-security.
const HIGH_SEC: f64 = 0.45;
const OUTLAW: f64 = -5.0;
const SQUAD: usize = 3;

/// Who has aggressed whom in a system, and who is wanted for it.
#[derive(Debug, Clone, Default)]
pub struct Watch {
    /// Aggressor character -> victim character or corporation -> when, ms.
    aggression: BTreeMap<i64, BTreeMap<i64, i64>>,
    /// Criminal character -> when the flag ends, and when CONCORD comes (if
    /// it has not yet).
    criminals: BTreeMap<i64, (i64, Option<i64>)>,
}

impl Watch {
    pub fn is_criminal(&self, char_id: i64, now: i64) -> bool {
        self.criminals.get(&char_id).is_some_and(|(until, _)| *until > now)
    }

    fn aggressed(&self, by: i64, on: i64, now: i64) -> bool {
        self.aggression.get(&by).and_then(|v| v.get(&on)).is_some_and(|t| now - t < AGGRESSION_MS)
    }

    /// Whether `by` aggressed this pilot or their corporation lately.
    fn aggressed_either(&self, by: i64, (char_id, corp_id): (i64, i64), now: i64) -> bool {
        self.aggressed(by, char_id, now) || self.aggressed(by, corp_id, now)
    }

    /// {aggressorID: {aggresseeID: when}}, as the client keeps it.
    pub fn aggressors(&self) -> Value {
        Value::Dict(
            self.aggression
                .iter()
                .map(|(by, on)| {
                    let inner = on.iter().map(|(v, t)| (Value::Int(*v), Value::Long(combat::filetime(*t)))).collect();
                    (Value::Int(*by), Value::Dict(inner))
                })
                .collect(),
        )
    }
}

/// The pilot behind a ship: its own, or a drone's owner.
fn pilot_of(ship: &Ship) -> Option<i64> {
    match &ship.drone {
        Some(d) => Some(d.owner),
        None if ship.is_player() => Some(ship.pilot.char_id),
        None => None,
    }
}

/// `attacker` did something offensive to `target`.
pub fn aggress(park: &mut Park, attacker: i64, target: i64, now: i64, out: &mut Out) {
    let (Some(by), Some(victim)) = (park.ships.get(&attacker), park.ships.get(&target)) else { return };
    let (Some(by_char), Some(on_char)) = (pilot_of(by), pilot_of(victim)) else { return };
    if by_char == on_char || victim.drone.is_some() {
        return;
    }
    let fair = park.watch.is_criminal(on_char, now)
        || park.watch.aggressed_either(on_char, (by_char, by.pilot.corp_id), now)
        || victim.pilot.security <= OUTLAW;
    let fresh = !park.watch.aggressed(by_char, on_char, now);
    park.watch.aggression.entry(by_char).or_default().insert(on_char, now);
    if fresh {
        let args = Value::Tuple(vec![Value::Int(park.solar_system_id), park.watch.aggressors()]);
        for ship in park.players() {
            out.note(ship.client_id, "OnAggressionChange", args.clone());
        }
    }
    if fair || !fresh {
        return;
    }
    let security = park.wildlife.security;
    if security >= HIGH_SEC {
        let response = (5.0 + (1.0 - security.clamp(0.5, 1.0)) * 40.0) as i64 * 1_000;
        let already = park.watch.criminals.get(&by_char).and_then(|(_, at)| *at);
        park.watch.criminals.insert(by_char, (now + AGGRESSION_MS, Some(already.unwrap_or(now + response))));
        out.jobs.push(Job::Security { char_id: by_char, delta: -0.5 * security });
        info!("{by_char} committed a crime against {on_char} in {}; CONCORD in {}s", park.solar_system_id, response / 1_000);
    } else if security > 0.0 {
        out.jobs.push(Job::Security { char_id: by_char, delta: -0.1 });
    }
}

/// A player's ship died: the killer pays in security status where that is a crime.
pub fn killed(park: &mut Park, ship: &Ship, capsule: bool, out: &mut Out) {
    let now = combat::now_ms();
    let Some(fight) = &ship.fight else { return };
    let Some((killer, killer_ship)) = fight.last_attacker.filter(|(c, _)| *c > 0 && *c != ship.pilot.char_id) else { return };
    let killer_corp = park.ships.get(&killer_ship).map_or(0, |s| s.pilot.corp_id);
    let victim = ship.pilot.char_id;
    let fair = park.watch.is_criminal(victim, now)
        || park.watch.aggressed_either(victim, (killer, killer_corp), now)
        || ship.pilot.security <= OUTLAW;
    let security = park.wildlife.security;
    if fair || security <= 0.0 {
        return;
    }
    let weight = if capsule { 3.0 } else { 1.5 };
    out.jobs.push(Job::Security { char_id: killer, delta: -weight * security.max(0.1) });
}

// ---- theft ------------------------------------------------------------------------------------

/// cfg.py:1401: the IDs of NPC characters and corporations.
fn is_npc(owner_id: i64) -> bool {
    owner_id > 10_000 && owner_id < 100_000_000
}

/// Whether a pilot of `corp_id` taking from a can of `owner_id` (of
/// `owner_corp`) is stealing: it is unless the can is theirs or their
/// corporation's, its owner shares their player corporation, or it belongs
/// to an NPC (consider.py:222).
fn is_theft(char_id: i64, corp_id: i64, owner_id: i64, owner_corp: i64) -> bool {
    owner_id > 0
        && !is_npc(owner_id)
        && owner_id != char_id
        && owner_id != corp_id
        && !(owner_corp == corp_id && !is_npc(corp_id))
}

/// The corporation of a character, or 0.
async fn corp_of(db: &tokio_postgres::Client, char_id: i64) -> i64 {
    match db.query_opt(r#"SELECT "corporationID" FROM character_ WHERE "characterID" = $1"#, &[&(char_id as i32)]).await {
        Ok(row) => row.map_or(0, |r| r.get::<_, i32>(0) as i64),
        Err(e) => {
            tracing::error!("reading the corporation of {char_id}: {}", crate::logging::chain(&e));
            0
        }
    }
}

/// Items left `origins`: each that is another pilot's can in `system` makes
/// the taker a thief, and the system is told who may now shoot them.
pub async fn took_from(db: &tokio_postgres::Client, world: &World, system: i64, char_id: i64, corp_id: i64, origins: &[i64]) {
    let cans: Vec<(i64, i64)> = {
        let w = world.lock();
        let Some(park) = w.parks.get(&system) else { return };
        origins.iter().filter_map(|id| park.cans.get(id)).map(|c| (c.id, c.owner_id)).collect()
    };
    for (can_id, owner_id) in cans {
        let owner_corp = corp_of(db, owner_id).await;
        if !is_theft(char_id, corp_id, owner_id, owner_corp) {
            continue;
        }
        let now = combat::now_ms();
        let mut w = world.lock();
        let Some(park) = w.parks.get_mut(&system) else { return };
        let flagged = park.watch.aggression.entry(char_id).or_default();
        flagged.insert(owner_id, now);
        if !is_npc(owner_corp) && owner_corp > 0 {
            flagged.insert(owner_corp, now);
        }
        let args = Value::Tuple(vec![Value::Int(system), park.watch.aggressors()]);
        let clients: Vec<i64> = park.players().map(|s| s.client_id).collect();
        w.notify(&clients, "OnAggressionChange", args);
        info!("{char_id} stole from can {can_id} of {owner_id}; fair game to them for 15 minutes");
    }
}

/// The bound aggressionMgr service (moniker.py:511).
pub async fn aggression_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    match call.method.as_str() {
        // Whether the caller may take from a can without stealing; the
        // client asks before warning that it would be theft.
        "CheckLootRightExceptions" => {
            let can_id = call.arg_i64(0)?;
            let (Some(system), Some(char_id)) = (ctx.session.solar_system_id, ctx.session.char_id) else {
                return Some(Outcome::Value(Value::Bool(true)));
            };
            let owner = ctx.world.lock().parks.get(&system).and_then(|p| p.cans.get(&can_id)).map(|c| c.owner_id);
            let Some(owner_id) = owner else { return Some(Outcome::Value(Value::Bool(true))) };
            let owner_corp = corp_of(ctx.db, owner_id).await;
            let corp_id = ctx.session.corp_id.unwrap_or(0);
            Some(Outcome::Value(Value::Bool(!is_theft(char_id, corp_id, owner_id, owner_corp))))
        }
        _ => None,
    }
}

/// CONCORD arrives when due, hunts its criminal, and leaves when done.
pub fn step(park: &mut Park, now: i64, out: &mut Out) {
    park.watch.criminals.retain(|_, (until, at)| *until > now || at.is_some());
    for v in park.watch.aggression.values_mut() {
        v.retain(|_, t| now - *t < AGGRESSION_MS);
    }
    park.watch.aggression.retain(|_, v| !v.is_empty());

    let due: Vec<i64> = park.watch.criminals.iter().filter(|(_, (_, at))| at.is_some_and(|t| t <= now)).map(|(c, _)| *c).collect();
    for criminal in due {
        if let Some(entry) = park.watch.criminals.get_mut(&criminal) {
            entry.1 = None;
        }
        arrive(park, criminal, now, out);
    }
    hunt(park, now, out);
}

/// The criminal's ship in this system, if it is here and not a capsule.
fn quarry(park: &Park, criminal: i64) -> Option<i64> {
    park.players().find(|s| s.pilot.char_id == criminal && s.pilot.group_id != 29).map(|s| s.id)
}

fn arrive(park: &mut Park, criminal: i64, now: i64, out: &mut Out) {
    let Some(target) = quarry(park, criminal) else { return };
    let Some(at) = park.ships.get(&target).map(|s| s.pos) else { return };
    let types = park.wildlife.police.clone();
    let Some(first) = types.first() else {
        info!("no CONCORD types to send for {criminal}");
        return;
    };
    for i in 0..SQUAD {
        let t = types.get(i % types.len()).unwrap_or(first);
        let offset = park.rng.in_sphere(5_000.0);
        let cop = crate::npc::police_ship(t, criminal, at + offset, now);
        let id = cop.id;
        park.ships.insert(id, cop);
        for e in space::add_ball_entries(park, id) {
            out.destiny(park.solar_system_id, e);
        }
        let flash = space::special_fx(park.tick, id, None, "effects.JumpIn");
        out.destiny(park.solar_system_id, flash);
    }
    info!("CONCORD arrives in {} for {criminal}", park.solar_system_id);
}

fn hunt(park: &mut Park, now: i64, out: &mut Out) {
    let cops: Vec<(i64, i64)> = park
        .ships
        .values()
        .filter_map(|s| s.npc.as_ref().and_then(|b| b.police).map(|p| (s.id, p.criminal)))
        .collect();
    let (tick, system) = (park.tick, park.solar_system_id);
    for (id, criminal) in cops {
        let target = quarry(park, criminal);
        let Some(cop) = park.ships.get_mut(&id) else { continue };
        let (Some(brain), Some(fight)) = (cop.npc.as_mut(), cop.fight.as_mut()) else { continue };
        let Some(police) = brain.police.as_mut() else { continue };
        let Some(t) = target else {
            // Nothing left to punish: stay a while, then go.
            if police.leave_ms <= now {
                if crate::fight::detach(park, id, now, out).is_some() {
                    out.destiny(system, entry(tick, "RemoveBalls", vec![Value::List(vec![Value::Int(id)])]));
                }
            }
            continue;
        };
        police.leave_ms = now + crate::npc::POLICE_LINGER_MS;
        brain.target = Some(t);
        if !fight.locks.contains_key(&t) {
            fight.locks.clear();
            fight.locks.insert(t, Lock::Pending(now + 1_000));
        }
        let mut started = Vec::new();
        if fight.is_locked(t) {
            for w in brain.arms() {
                if fight.module(w.module_id).is_none() {
                    let mut m = w.clone();
                    m.target = Some(t);
                    m.started_ms = now;
                    m.next_ms = now;
                    started.push(m.clone());
                    fight.modules.push(m);
                } else if let Some(m) = fight.module_mut(w.module_id) {
                    m.target = Some(t);
                }
            }
        }
        for m in &started {
            out.destiny(system, crate::fight::module_fx(tick, id, m, true));
        }
        let course = Command::Orbit { target: t, range: 2_500.0 };
        if cop.command != course {
            park.command(id, course);
            if let Some(s) = park.ships.get(&id) {
                for e in space::course_entries(s, tick) {
                    out.destiny(system, e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYER_CORP: i64 = 140_000_500;
    const NPC_CORP: i64 = 1_000_172;

    #[test]
    fn theft() {
        assert!(!is_theft(140_000_001, NPC_CORP, 140_000_001, NPC_CORP), "your own can");
        assert!(!is_theft(140_000_001, PLAYER_CORP, 140_000_002, PLAYER_CORP), "a corpmate's can");
        assert!(is_theft(140_000_001, NPC_CORP, 140_000_002, NPC_CORP), "NPC corps share nothing");
        assert!(is_theft(140_000_001, PLAYER_CORP, 140_000_002, NPC_CORP), "a stranger's can");
        assert!(!is_theft(140_000_001, NPC_CORP, 1_000_125, 0), "an NPC's can");
    }

    #[test]
    fn a_thief_is_fair_game_to_the_owner_corp() {
        let mut watch = Watch::default();
        watch.aggression.entry(140_000_001).or_default().insert(PLAYER_CORP, 0);
        assert!(watch.aggressed_either(140_000_001, (140_000_009, PLAYER_CORP), 1_000));
        assert!(!watch.aggressed_either(140_000_001, (140_000_009, NPC_CORP), 1_000));
        assert!(!watch.aggressed_either(140_000_001, (140_000_009, PLAYER_CORP), AGGRESSION_MS + 1));
    }
}
