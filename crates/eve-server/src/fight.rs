//! One pulse of fighting in one solar system, under the world lock: locks
//! complete or break, modules cycle, missiles land, rats think, and whatever
//! was destroyed leaves the park. What needs the database (ammunition, ore,
//! bounties, a lost ship) is handed back to the heartbeat as jobs.
//!
//! A module cycle does its work when it starts (a shot, a repair), except
//! mining, which delivers at the end. Between cycles the client is told
//! nothing: it saw the module start (OnGodmaShipEffect to the pilot,
//! OnSpecialFX to everyone) and repeats it until it hears it stop.

use eve_marshal::Value;
use tracing::{debug, info};

use std::collections::BTreeMap;

use crate::ballpark::{Command, Park, Ship};
use crate::combat::{self, attr, Action, Active, Afflictions, Damage, Lock, Shot, REPEAT_FOREVER};
use crate::heartbeat::{Job, Out};
use crate::npc::combat_ref::Attacker;
use crate::npc::{self, Missile};
use crate::svc::space::entry;

/// Locks hold a little past the locking range, as the client's and our
/// simulations drift.
const LOCK_SLACK: f64 = 1.1;
/// Mining reaches a little past its range, for the same reason.
const RANGE_SLACK: f64 = 1.1;
const GROUP_CAPSULE: i64 = 29;

pub fn step(park: &mut Park, now: i64, out: &mut Out) {
    locks(park, now, out);
    cycles(park, now, out);
    missiles(park, now, out);
    afflict(park, out);
    npc::think(park, now, out);
    crate::drones::think(park, now, out);
    crate::concord::step(park, now, out);
    deaths(park, now, out);
    crate::loot::expire(park, now, out);
    report(park, now, out);
}

// ---- locks ------------------------------------------------------------------------------

fn locks(park: &mut Park, now: i64, out: &mut Out) {
    let ids: Vec<i64> = park.ships.keys().copied().collect();
    for id in ids {
        let Some(ship) = park.ships.get(&id) else { continue };
        let Some(fight) = &ship.fight else { continue };
        let mut changes = Vec::new();
        for (&t, &lock) in &fight.locks {
            let gone = match park.ships.get(&t) {
                Some(target) => target.in_warp(),
                None => !park.rocks.contains_key(&t),
            };
            let far = park.surface_distance(id, t).map_or(true, |d| d > fight.lock_range() * LOCK_SLACK);
            if gone || far || ship.in_warp() || fight.afflictions.jammed {
                changes.push((t, lock, None));
            } else if let Lock::Pending(ready) = lock {
                if now >= ready {
                    changes.push((t, lock, Some(Lock::Locked)));
                }
            }
        }
        for (t, was, now_lock) in changes {
            if let Some(f) = park.ships.get_mut(&id).and_then(|s| s.fight.as_mut()) {
                match now_lock {
                    Some(l) => {
                        f.locks.insert(t, l);
                    }
                    None => {
                        f.locks.remove(&t);
                    }
                }
            }
            match now_lock {
                Some(_) => {
                    tell(park, id, out, "OnTarget", Value::Tuple(vec![Value::bytes("add"), Value::Int(t)]));
                    tell(park, t, out, "OnTarget", Value::Tuple(vec![Value::bytes("otheradd"), Value::Int(id)]));
                    debug!("{id} locked {t}");
                }
                None => {
                    let lost = Value::Tuple(vec![Value::bytes("lost"), Value::Int(t), Value::None]);
                    tell(park, id, out, "OnTarget", lost);
                    if was == Lock::Locked {
                        tell_target_lost(park, id, t, out);
                    }
                }
            }
        }
    }
}

/// Tells `target`'s pilot that `locker` no longer has it locked.
pub fn tell_target_lost(park: &Park, locker: i64, target: i64, out: &mut Out) {
    let lost = Value::Tuple(vec![Value::bytes("otherlost"), Value::Int(locker), Value::None]);
    tell(park, target, out, "OnTarget", lost);
}

/// A notification for the pilot of `ship_id`, if a player flies it.
fn tell(park: &Park, ship_id: i64, out: &mut Out, event: &'static str, args: Value) {
    if let Some(s) = park.ships.get(&ship_id).filter(|s| s.is_player()) {
        out.note(s.client_id, event, args);
    }
}

// ---- module cycles ---------------------------------------------------------------------------

fn cycles(park: &mut Park, now: i64, out: &mut Out) {
    let due: Vec<(i64, i64)> = park
        .ships
        .values()
        .filter_map(|s| s.fight.as_ref().map(|f| (s.id, f)))
        .flat_map(|(id, f)| f.modules.iter().filter(|m| m.next_ms <= now).map(move |m| (id, m.module_id)))
        .collect();
    for (ship, module) in due {
        cycle(park, ship, module, now, out);
    }
}

fn cycle(park: &mut Park, ship_id: i64, module_id: i64, now: i64, out: &mut Out) {
    let Some(m) = park.ships.get(&ship_id).and_then(|s| s.fight.as_ref()).and_then(|f| f.module(module_id)).cloned()
    else {
        return;
    };

    // What the cycle that just ended gathered.
    if m.cycles > 0 {
        if let Action::Mining { amount, range } = m.action {
            mine(park, ship_id, &m, amount, range, out);
        }
    }
    if let Some(why) = stop_reason(park, ship_id, &m) {
        stop(park, ship_id, module_id, now, out, why);
        return;
    }

    let Some(ship) = park.ships.get_mut(&ship_id) else { return };
    let player = ship.is_player().then_some((ship.client_id, ship.pilot.char_id));
    let Some(fight) = ship.fight.as_mut() else { return };
    if m.cap_need > 0.0 {
        if !fight.vitals.capacitor.spend(now, m.cap_need) {
            stop(park, ship_id, module_id, now, out, Some(notify("Your capacitor is too low to run that module.")));
            return;
        }
        out.drained(ship_id);
    }
    let Some(running) = fight.module_mut(module_id) else { return };
    if let Some(ammo) = running.ammo.as_mut() {
        ammo.quantity -= ammo.per_cycle;
        if let Some((client_id, _)) = player {
            out.jobs.push(Job::Ammo { client_id, charge_id: ammo.item_id, quantity: ammo.quantity });
        }
    }
    running.cycles += 1;
    // A pulse late is not a cycle lost, but a long stall does not bank cycles.
    running.next_ms = (running.next_ms + running.duration_ms).max(now + running.duration_ms / 2);

    match &m.action {
        Action::Turret { damage, gun } => shoot(park, ship_id, &m, *damage, gun, now, out),
        Action::Missile { damage, flight_ms, range, cloud, aoe_velocity } => {
            launch(park, ship_id, &m, *damage, *flight_ms, *range, *cloud, *aoe_velocity, now)
        }
        Action::ShieldBoost(amount) => heal(park, ship_id, now, out, |v| v.shield.add(now, *amount)),
        Action::ArmorRepair(amount) => heal(park, ship_id, now, out, |v| v.repair_armor(*amount)),
        Action::HullRepair(amount) => heal(park, ship_id, now, out, |v| v.repair_hull(*amount)),
        Action::CapBoost(amount) => {
            heal(park, ship_id, now, out, |v| v.capacitor.add(now, *amount));
            out.drained(ship_id);
        }
        Action::Mining { .. } | Action::Passive => {}
        _ => project(park, ship_id, &m, now, out),
    }
}

/// Why a module should not start another cycle; Some(None) stops it quietly.
fn stop_reason(park: &Park, ship_id: i64, m: &Active) -> Option<Option<(String, Value)>> {
    let ship = park.ships.get(&ship_id)?;
    let fight = ship.fight.as_ref()?;
    if m.stopping || ship.in_warp() {
        return Some(None);
    }
    if m.repeat < REPEAT_FOREVER && m.cycles > m.repeat {
        return Some(None);
    }
    if let Some(ammo) = &m.ammo {
        if ammo.quantity < ammo.per_cycle {
            return Some(None);
        }
    }
    if let Some(t) = m.target {
        if !fight.is_locked(t) {
            return Some(None);
        }
        if let Action::Mining { range, .. } = m.action {
            let distance = park.surface_distance(ship_id, t).unwrap_or(f64::MAX);
            if distance > range * RANGE_SLACK {
                return Some(Some(notify(&format!(
                    "Your {} deactivates: the asteroid is {:.0} m away, beyond its range of {:.0} m.",
                    m.name, distance, range
                ))));
            }
        }
        if let Some((optimal, falloff)) = m.reach {
            let reach = if falloff > 0.0 { optimal + 3.0 * falloff } else { optimal * RANGE_SLACK };
            let distance = park.surface_distance(ship_id, t).unwrap_or(f64::MAX);
            if distance > reach {
                return Some(ship.is_player().then(|| {
                    notify(&format!("Your {} deactivates: the target is {distance:.0} m away, out of its reach.", m.name))
                }));
            }
        }
    }
    None
}

/// A projected module's cycle: whether it takes hold this time (falloff, and
/// for a jammer its strength against the target's sensors), and what it does
/// at once if it does.
fn project(park: &mut Park, ship_id: i64, m: &Active, now: i64, out: &mut Out) {
    let Some(t) = m.target else { return };
    let (optimal, falloff) = m.reach.unwrap_or((0.0, 0.0));
    let distance = park.surface_distance(ship_id, t).unwrap_or(f64::MAX);
    let mut landed = park.rng.unit() < combat::falloff_chance(distance, optimal, falloff);
    if let Action::Jam(strength) = m.action {
        let sensors = park.ships.get(&t).and_then(|s| s.fight.as_ref()).map_or([0.0; 4], |f| f.sensors);
        landed = landed && park.rng.unit() < combat::jam_chance(strength, sensors);
        if landed {
            let jammer = park.ships.get(&ship_id).map(|s| s.pilot.name.clone()).unwrap_or_default();
            let args = Value::Dict(vec![(Value::bytes("jammer"), Value::text(jammer))]);
            tell(park, t, out, "OnRemoteMessage", Value::Tuple(vec![Value::bytes("TargetJammedBy"), args]));
        }
    }
    if let Some(running) = park.ships.get_mut(&ship_id).and_then(|s| s.fight.as_mut()).and_then(|f| f.module_mut(m.module_id)) {
        running.landed = landed;
    }
    if m.offensive {
        aggress(park, ship_id, t, now, out);
    }
    if !landed {
        return;
    }
    // Capacitor moves both ways for a leech, so take the target's first.
    let mut leeched = 0.0;
    if let Some(target) = park.ships.get_mut(&t).and_then(|s| s.fight.as_mut()) {
        let v = &mut target.vitals;
        match m.action {
            Action::Neutralize(amount) => {
                let take = amount.min(v.capacitor.at(now));
                v.capacitor.add(now, -take);
                out.drained(t);
            }
            Action::Leech(amount) => {
                leeched = amount.min(v.capacitor.at(now));
                v.capacitor.add(now, -leeched);
                out.drained(t);
            }
            Action::GiveCapacitor(amount) => {
                v.capacitor.add(now, amount);
                out.drained(t);
            }
            Action::GiveShield(amount) => {
                v.shield.add(now, amount);
                out.hurt(t);
            }
            Action::GiveArmor(amount) => {
                v.repair_armor(amount);
                out.hurt(t);
            }
            _ => {}
        }
    }
    if leeched > 0.0 {
        if let Some(me) = park.ships.get_mut(&ship_id).and_then(|s| s.fight.as_mut()) {
            me.vitals.capacitor.add(now, leeched);
            out.drained(ship_id);
        }
    }
}

/// Someone attacked `target`: a rat fights back; a pilot may have committed
/// a crime.
fn aggress(park: &mut Park, attacker: i64, target: i64, now: i64, out: &mut Out) {
    if let Some(brain) = park.ships.get_mut(&target).and_then(|s| s.npc.as_mut()) {
        brain.provoked(attacker);
    }
    crate::concord::aggress(park, attacker, target, now, out);
}

/// What everyone's webs, scramblers, jammers, dampeners, disruptors and
/// painters are doing to each ship this pulse, and what follows: a webbed ship
/// slows (and everyone is told its new top speed), a scrambled one aligning to
/// warp is stopped.
fn afflict(park: &mut Park, out: &mut Out) {
    let mut on: BTreeMap<i64, Vec<Action>> = BTreeMap::new();
    for ship in park.ships.values() {
        let Some(fight) = &ship.fight else { continue };
        for m in fight.modules.iter().filter(|m| m.landed && m.cycles > 0) {
            if let (Some(t), Action::Web(_) | Action::Scramble(_) | Action::Jam(_) | Action::Dampen { .. } | Action::Disrupt { .. } | Action::Paint(_)) =
                (m.target, &m.action)
            {
                on.entry(t).or_default().push(m.action.clone());
            }
        }
    }
    let ids: Vec<i64> = park.ships.keys().copied().collect();
    let (tick, system) = (park.tick, park.solar_system_id);
    for id in ids {
        let effects = on.remove(&id).unwrap_or_default();
        let stack = |pick: &dyn Fn(&Action) -> Option<f64>| {
            let factors: Vec<f64> = effects.iter().filter_map(pick).collect();
            crate::dogma::stacking_penalised(&factors)
        };
        let now = Afflictions {
            speed: stack(&|a| if let Action::Web(f) = a { Some(*f) } else { None }),
            signature: stack(&|a| if let Action::Paint(f) = a { Some(*f) } else { None }),
            scan_resolution: stack(&|a| if let Action::Dampen { resolution, .. } = a { Some(*resolution) } else { None }),
            lock_range: stack(&|a| if let Action::Dampen { range, .. } = a { Some(*range) } else { None }),
            optimal: stack(&|a| if let Action::Disrupt { range, .. } = a { Some(*range) } else { None }),
            tracking: stack(&|a| if let Action::Disrupt { tracking, .. } = a { Some(*tracking) } else { None }),
            scrambled: effects.iter().map(|a| if let Action::Scramble(s) = a { *s } else { 0.0 }).sum(),
            jammed: effects.iter().any(|a| matches!(a, Action::Jam(_))),
        };
        let Some(ship) = park.ships.get_mut(&id) else { continue };
        let Some(fight) = ship.fight.as_mut() else { continue };
        let was = std::mem::replace(&mut fight.afflictions, now);
        if fight.top_speed > 0.0 {
            let speed = fight.top_speed * now.speed;
            if (ship.stats.max_velocity - speed).abs() > 1e-3 {
                ship.stats.max_velocity = speed;
                out.destiny(system, entry(tick, "SetMaxSpeed", vec![Value::Int(id), Value::Double(speed)]));
            }
        }
        let player = ship.is_player().then_some(ship.client_id);
        let aligning = ship.warping() && !ship.in_warp();
        if now.scrambled >= 1.0 && aligning {
            park.command(id, Command::Stop);
            out.destiny(system, entry(tick, "Stop", vec![Value::Int(id)]));
            if let Some(client) = player {
                out.note(client, "OnRemoteMessage", Value::Tuple(vec![Value::bytes("WarpScrambled"), Value::Dict(Vec::new())]));
            }
        }
        if now.jammed && !was.jammed {
            debug!("{id} jammed");
        }
    }
}

/// A CustomNotify message.
pub fn notify(text: &str) -> (String, Value) {
    ("CustomNotify".to_string(), Value::Dict(vec![(Value::bytes("notify"), Value::text(text))]))
}

/// Stops a running module and tells everyone.
pub fn stop(park: &mut Park, ship_id: i64, module_id: i64, now: i64, out: &mut Out, message: Option<(String, Value)>) {
    let tick = park.tick;
    let system = park.solar_system_id;
    let Some(ship) = park.ships.get_mut(&ship_id) else { return };
    let Some(fight) = ship.fight.as_mut() else { return };
    let Some(at) = fight.modules.iter().position(|m| m.module_id == module_id) else { return };
    let m = fight.modules.remove(at);
    if ship.is_player() {
        out.note(ship.client_id, "OnGodmaShipEffect", effect_event(ship.pilot.char_id, ship_id, &m, false, now));
        if let Some((key, args)) = message {
            out.note(ship.client_id, "OnRemoteMessage", Value::Tuple(vec![Value::bytes(key), args]));
        }
        if m.action == Action::Passive {
            out.jobs.push(Job::Refit { client_id: ship.client_id, char_id: ship.pilot.char_id, ship_id, system });
        }
    }
    out.destiny(system, module_fx(tick, ship_id, &m, false));
}

/// OnGodmaShipEffect(itemID, effectID, time, start, active, environment,
/// startTime, duration, repeat, randomSeed, error) for a running module.
pub fn effect_event(char_id: i64, ship_id: i64, m: &Active, start: bool, now: i64) -> Value {
    let environment = Value::List(vec![
        Value::Int(m.module_id),
        Value::Int(char_id),
        Value::Int(ship_id),
        Value::int_or_none(m.target),
        Value::int_or_none(m.ammo.map(|a| a.item_id)),
        Value::List(Vec::new()),
        Value::Int(m.effect_id),
    ]);
    Value::Tuple(vec![
        Value::Int(m.module_id),
        Value::Int(m.effect_id),
        Value::Long(combat::filetime(now)),
        Value::Int(i64::from(start)),
        Value::Int(i64::from(start)),
        environment,
        Value::Long(combat::filetime(m.started_ms)),
        Value::Int(m.duration_ms),
        Value::Int(m.repeat),
        Value::None,
        Value::None,
    ])
}

/// OnSpecialFX(shipID, moduleID, moduleTypeID, targetID, otherTypeID, area,
/// guid, isOffensive, start, active, duration, repeat, startTime): what
/// everyone in the system sees of a module.
pub fn module_fx(tick: i64, ship_id: i64, m: &Active, start: bool) -> Value {
    entry(
        tick,
        "OnSpecialFX",
        vec![
            Value::Int(ship_id),
            Value::Int(m.module_id),
            Value::Int(m.type_id),
            Value::int_or_none(m.target),
            Value::int_or_none(m.ammo.map(|a| a.type_id)),
            Value::List(Vec::new()),
            Value::bytes(m.guid.as_str()),
            Value::Int(i64::from(m.offensive)),
            Value::Int(i64::from(start)),
            Value::Int(i64::from(start)),
            Value::Int(m.duration_ms),
            Value::Int(m.repeat),
            Value::Long(combat::filetime(m.started_ms)),
        ],
    )
}

fn attacker(ship: &Ship, weapon: &Active) -> Attacker {
    Attacker {
        ship_id: ship.id,
        char_id: ship.pilot.char_id,
        // A drone's pilot hears what it hits.
        client_id: match &ship.drone {
            Some(d) => Some(d.client_id),
            None => ship.is_player().then_some(ship.client_id),
        },
        ship_type: ship.pilot.type_id,
        weapon_type: weapon.type_id,
    }
}

fn shoot(park: &mut Park, ship_id: i64, m: &Active, damage: Damage, gun: &combat::Gun, now: i64, out: &mut Out) {
    let Some(t) = m.target else { return };
    let (Some(shooter), Some(target)) = (park.ships.get(&ship_id), park.ships.get(&t)) else { return };
    let Some(signature) = target.fight.as_ref().map(|f| f.sig()) else { return };
    let Some(distance) = park.surface_distance(ship_id, t) else { return };
    let angular = combat::angular_velocity(shooter.pos, shooter.vel, target.pos, target.vel);
    // A tracking disruptor on the shooter shortens and slows its guns.
    let disrupted = shooter.fight.as_ref().map_or(Afflictions::default(), |f| f.afflictions);
    let gun = combat::Gun {
        optimal: gun.optimal * disrupted.optimal,
        tracking: gun.tracking * disrupted.tracking,
        ..*gun
    };
    let chance = combat::hit_chance(&gun, distance.max(0.0), angular, signature);
    let by = attacker(shooter, m);
    let shot = combat::turret_shot(chance, park.rng.unit(), park.rng.unit());
    hit(park, by, t, damage.map(|d| d * shot.multiplier()), shot, &m.name, now, out);
}

#[allow(clippy::too_many_arguments)]
fn launch(
    park: &mut Park,
    ship_id: i64,
    m: &Active,
    damage: Damage,
    flight_ms: i64,
    range: f64,
    cloud: f64,
    aoe_velocity: f64,
    now: i64,
) {
    let Some(t) = m.target else { return };
    let Some(shooter) = park.ships.get(&ship_id) else { return };
    let Some(distance) = park.surface_distance(ship_id, t) else { return };
    let speed = if flight_ms > 0 { range / flight_ms as f64 } else { 1.0 };
    let reaches = distance <= range;
    let impact_ms = now + (distance.max(0.0) / speed.max(1e-3)).min(flight_ms.max(0) as f64) as i64;
    park.missiles.push(Missile {
        attacker: attacker(shooter, m),
        target: t,
        impact_ms,
        damage,
        cloud,
        aoe_velocity,
        reaches,
        weapon: m.name.clone(),
    });
}

fn missiles(park: &mut Park, now: i64, out: &mut Out) {
    let (landed, flying): (Vec<Missile>, Vec<Missile>) =
        std::mem::take(&mut park.missiles).into_iter().partition(|m| m.impact_ms <= now);
    park.missiles = flying;
    for m in landed {
        if !m.reaches {
            continue;
        }
        let Some(target) = park.ships.get(&m.target) else { continue };
        let Some(signature) = target.fight.as_ref().map(|f| f.sig()) else { continue };
        let factor = combat::missile_factor(m.cloud, m.aoe_velocity, signature, target.vel.len());
        let quality = match factor {
            f if f >= 0.99 => 3,
            f if f >= 0.5 => 2,
            _ => 1,
        };
        let shot = Shot::Hit { quality, multiplier: factor };
        hit(park, m.attacker, m.target, m.damage.map(|d| d * factor), shot, &m.weapon, now, out);
    }
}

/// Puts a shot's damage on its target and tells both sides how it went.
#[allow(clippy::too_many_arguments)]
fn hit(park: &mut Park, by: Attacker, target: i64, damage: Damage, shot: Shot, weapon: &str, now: i64, out: &mut Out) {
    let Some(victim) = park.ships.get_mut(&target) else { return };
    let Some(fight) = victim.fight.as_mut() else { return };
    let dealt = match shot {
        Shot::Hit { .. } => fight.vitals.apply(now, damage).total,
        Shot::Miss(_) => 0.0,
    };
    fight.last_attacker = Some((by.char_id, by.ship_id));
    if dealt > 0.0 {
        // A pilot by character, an NPC by minus its type.
        let who = if by.char_id != 0 { by.char_id } else { -by.ship_type };
        let entry = fight.damage_log.entry(who).or_insert((by.ship_type, by.weapon_type, 0.0));
        *entry = (by.ship_type, by.weapon_type, entry.2 + dealt);
    }
    let victim_client = victim.is_player().then_some(victim.client_id);
    if dealt > 0.0 {
        out.hurt(target);
    }

    let key = match shot {
        Shot::Miss(q) => format!("AttackMiss{q}"),
        Shot::Hit { quality, .. } => format!("AttackHit{quality}"),
    };
    let message = |key: String, pairs: Vec<(&str, Value)>| {
        let args = Value::Dict(pairs.into_iter().map(|(k, v)| (Value::bytes(k), v)).collect());
        Value::Tuple(vec![Value::bytes(key), args])
    };
    if let Some(client) = by.client_id {
        let pairs = vec![
            ("weapon", Value::text(weapon)),
            ("target", Value::Int(target)),
            ("damage", Value::Double(dealt)),
        ];
        out.note(client, "OnDamageMessage", message(key.clone(), pairs));
    }
    if let Some(client) = victim_client {
        let pairs = vec![("source", Value::Int(by.ship_id)), ("damage", Value::Double(dealt))];
        out.note(client, "OnDamageMessage", message(format!("{key}R"), pairs));
    }
    aggress(park, by.ship_id, target, now, out);
}

fn heal(park: &mut Park, ship_id: i64, _now: i64, out: &mut Out, f: impl FnOnce(&mut combat::Vitals)) {
    if let Some(fight) = park.ships.get_mut(&ship_id).and_then(|s| s.fight.as_mut()) {
        f(&mut fight.vitals);
        out.hurt(ship_id);
    }
}

fn mine(park: &mut Park, ship_id: i64, m: &Active, amount: f64, range: f64, out: &mut Out) {
    let Some(t) = m.target else { return };
    let Some(ship) = park.ships.get(&ship_id) else { return };
    if park.surface_distance(ship_id, t).map_or(true, |d| d > range * RANGE_SLACK) {
        return;
    }
    // Ore goes into the miner's hold, or a mining drone's ship's.
    let (client_id, char_id, hold) = match &ship.drone {
        Some(d) => match d.controller {
            Some(c) => (d.client_id, d.owner, c),
            None => return,
        },
        None if ship.is_player() => (ship.client_id, ship.pilot.char_id, ship_id),
        None => return,
    };
    let Some(rock) = park.rocks.get_mut(&t) else { return };
    let units = ((amount / rock.volume).floor() as i64).clamp(0, rock.quantity);
    if units == 0 {
        return;
    }
    rock.quantity -= units;
    out.jobs.push(Job::Ore {
        client_id,
        char_id,
        ship_id: hold,
        miner_id: ship_id,
        module_id: m.module_id,
        system: park.solar_system_id,
        type_id: rock.type_id,
        quantity: units,
    });
    if rock.quantity == 0 {
        park.rocks.remove(&t);
        info!("asteroid {t} mined out");
        out.destiny(park.solar_system_id, entry(park.tick, "RemoveBalls", vec![Value::List(vec![Value::Int(t)])]));
    }
}

// ---- losses ---------------------------------------------------------------------------------

fn deaths(park: &mut Park, now: i64, out: &mut Out) {
    let dead: Vec<i64> = park
        .ships
        .values()
        .filter(|s| s.fight.as_ref().is_some_and(|f| f.vitals.hull_max > 0.0 && f.vitals.hull_damage >= f.vitals.hull_max - 1e-6))
        .map(|s| s.id)
        .collect();
    for id in dead {
        let system = park.solar_system_id;
        // The explosion goes in the same update as the removal, so the client
        // blows the ball up instead of just dropping it (michelle.py:640).
        out.destiny(system, entry(park.tick, "TerminalExplosion", vec![Value::Int(id)]));
        out.destiny(system, entry(park.tick, "RemoveBalls", vec![Value::List(vec![Value::Int(id)])]));
        let Some(ship) = detach(park, id, now, out) else { continue };
        let killer = ship.fight.as_ref().and_then(|f| f.last_attacker);
        if ship.npc.is_some() {
            npc::killed(park, &ship, now);
            let bounty = ship.npc.as_ref().map_or(0.0, |b| b.bounty);
            info!("{} ({id}) destroyed by {}, bounty {bounty}", ship.pilot.name, culprit(park, killer));
            let brain = ship.npc.as_ref().expect("an NPC has a brain");
            if let Some((char_id, _)) = killer.filter(|(c, _)| *c != 0 && brain.police.is_none()) {
                let client_id = park.players().find(|s| s.pilot.char_id == char_id).map(|s| s.client_id);
                if bounty > 0.0 {
                    out.jobs.push(Job::Bounty {
                        client_id,
                        char_id,
                        amount: bounty,
                        npc_type: ship.pilot.type_id,
                        security: (brain.security_bonus, brain.security_max_gain),
                    });
                }
                let stash = crate::loot::roll(bounty, brain.loot_count, &mut park.rng);
                if !stash.is_empty() {
                    out.jobs.push(Job::Loot { system, pos: ship.pos, owner_id: char_id, stash });
                }
            }
        } else if let Some(drone) = &ship.drone {
            info!("drone {id} of {} destroyed by {}", drone.owner, culprit(park, killer));
            out.jobs.push(Job::DroneLost { client_id: Some(drone.client_id), drone_id: id });
        } else {
            info!("{}'s ship {id} destroyed by {}", ship.pilot.name, culprit(park, killer));
            let capsule = ship.pilot.group_id == GROUP_CAPSULE;
            crate::concord::killed(park, &ship, capsule, out);
            out.jobs.push(Job::Lost(Box::new(crate::losses::Loss::of(&ship, system, capsule))));
        }
    }
}

/// Who `last_attacker` was, for the log: the ship's name if it is still
/// here, else its pilot or ship ID.
fn culprit(park: &Park, killer: Option<(i64, i64)>) -> String {
    match killer {
        None => "unknown causes".to_string(),
        Some((char_id, ship_id)) => match park.ships.get(&ship_id) {
            Some(s) => format!("{} ({ship_id})", s.pilot.name),
            None if char_id != 0 => format!("character {char_id}"),
            None => format!("ship {ship_id}"),
        },
    }
}

/// A ship about to leave (dock, jump, log off): its modules stop and its
/// locks go.
pub fn stand_down(park: &mut Park, ship_id: i64, now: i64, out: &mut Out) {
    let modules: Vec<i64> = park
        .ships
        .get(&ship_id)
        .and_then(|s| s.fight.as_ref())
        .map(|f| f.modules.iter().map(|m| m.module_id).collect())
        .unwrap_or_default();
    for m in modules {
        stop(park, ship_id, m, now, out, None);
    }
    let Some(fight) = park.ships.get_mut(&ship_id).and_then(|s| s.fight.as_mut()) else { return };
    let dropped: Vec<i64> = std::mem::take(&mut fight.locks).into_keys().collect();
    for t in dropped {
        tell_target_lost(park, ship_id, t, out);
    }
}

/// Takes a ship out of the park and out of everyone's fight: whoever had it
/// locked loses it, modules aimed at it stop, missiles at it fizzle, and
/// whoever it had locked hears it let go.
pub fn detach(park: &mut Park, id: i64, now: i64, out: &mut Out) -> Option<Ship> {
    let ship = park.ships.remove(&id)?;
    let mut stops = Vec::new();
    for other in park.ships.values_mut() {
        if let Some(brain) = other.npc.as_mut() {
            if brain.target == Some(id) {
                brain.target = None;
            }
        }
        let Some(fight) = other.fight.as_mut() else { continue };
        fight.locks.remove(&id);
        stops.extend(fight.modules.iter().filter(|m| m.target == Some(id)).map(|m| (other.id, m.module_id)));
    }
    for (ship_id, module_id) in stops {
        stop(park, ship_id, module_id, now, out, None);
    }
    park.missiles.retain(|m| m.target != id);
    if let Some(fight) = &ship.fight {
        for t in fight.locked() {
            tell_target_lost(park, id, t, out);
        }
    }
    Some(ship)
}

// ---- telling -------------------------------------------------------------------------------

/// OnDamageStateChange to the system for everyone hurt (or healed) this pulse,
/// and the pilot's own gauges (and capacitor) as attribute changes.
fn report(park: &Park, now: i64, out: &mut Out) {
    let (hurt, drained) = out.take_marks(|id| park.ships.contains_key(&id));
    for id in &hurt {
        let Some(ship) = park.ships.get(id) else { continue };
        let Some(fight) = &ship.fight else { continue };
        out.destiny(park.solar_system_id, entry(park.tick, "OnDamageStateChange", vec![Value::Int(*id), damage_state(&fight.vitals, now)]));
    }
    for ship in park.players() {
        let Some(fight) = &ship.fight else { continue };
        let mut changes = Vec::new();
        let change = |a: i32, v: f64| {
            Value::Tuple(vec![
                Value::bytes("OnModuleAttributeChange"),
                Value::Int(ship.pilot.char_id),
                Value::Int(ship.id),
                Value::Int(a as i64),
                Value::Long(combat::filetime(now)),
                Value::Double(v),
                Value::Double(v),
            ])
        };
        if hurt.contains(&ship.id) {
            changes.push(change(attr::SHIELD_CHARGE, fight.vitals.shield.at(now)));
            changes.push(change(attr::ARMOR_DAMAGE, fight.vitals.armor_damage));
            changes.push(change(attr::DAMAGE, fight.vitals.hull_damage));
        }
        if hurt.contains(&ship.id) || drained.contains(&ship.id) {
            changes.push(change(attr::CHARGE, fight.vitals.capacitor.at(now)));
        }
        if !changes.is_empty() {
            out.note(ship.client_id, "OnModuleAttributeChanges", Value::Tuple(vec![Value::List(changes)]));
        }
    }
}

/// [(shield fraction, shield tau), armour fraction, structure fraction].
pub fn damage_state(v: &combat::Vitals, now: i64) -> Value {
    let (shield, tau, armor, hull) = v.damage_state(now);
    Value::List(vec![Value::Tuple(vec![Value::Double(shield), Value::Double(tau)]), Value::Double(armor), Value::Double(hull)])
}
