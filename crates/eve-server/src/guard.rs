//! Starbase defences: the sentry guns, missile batteries and electronic
//! warfare batteries anchored at a control tower.
//!
//! An online defence whose tower is online watches the ships within its
//! proximityRange and engages the nearest one the tower's sentry settings
//! (moonmining's Defense tab) make hostile:
//!
//! - a standing from the owner's corporation or alliance below the set level;
//! - a security status below the set level;
//! - "our standing is dropping": any negative standing from the owner;
//! - "other security status is dropping": a pilot flagged a criminal;
//! - aggression: a pilot who lately attacked the owner's corporation or one
//!   of its members here;
//! - war: a pilot whose side is at war with the owner's.
//!
//! The owner, its corporation and its alliance are never engaged. A new
//! target takes the battery's targetSwitchDelay to acquire; then guns and
//! missile batteries fire each cycle with what is loaded in their bay
//! (guns' crystals last, everything else is used up a round a shot), and EW
//! batteries try their web, scrambler, jammer or damper each cycle, which
//! fight.rs's afflict applies as it does a ship's.

use std::collections::HashMap;
use std::sync::RwLock;

use tokio_postgres::GenericClient;
use tracing::{debug, error};

use crate::ballpark::{Park, Pilot, Ship, Vec3};
use crate::combat::{self, attr, Action, Damage, Gun};
use crate::heartbeat::{Job, Out};
use crate::npc::combat_ref::Attacker;
use crate::npc::Missile;
use crate::pos::Structure;
use crate::svc::rows::{float, int};

const GROUP_MISSILE_SENTRY: i64 = 417;
const GROUP_PROJECTILE_SENTRY: i64 = 426;
const GROUP_LASER_SENTRY: i64 = 430;
const GROUP_HYBRID_SENTRY: i64 = 449;
const GROUP_EW_BATTERY: i64 = 439;
const GROUP_DAMPENING_BATTERY: i64 = 440;
const GROUP_WEB_BATTERY: i64 = 441;
const GROUP_SCRAMBLE_BATTERY: i64 = 443;
const GROUP_FREQUENCY_CRYSTAL: i64 = 86;
const CATEGORY_CHARGE: i32 = 8;

const ATTR_SPEED: i32 = 51;
const ATTR_PROXIMITY_RANGE: i32 = 154;
const ATTR_TARGET_SWITCH_DELAY: i32 = 691;
const ATTR_MISSILE_LAUNCH_DURATION: i32 = 506;
const ATTR_DURATION: i32 = 73;
const ATTR_SPEED_FACTOR: i32 = 20;
const ATTR_WARP_SCRAMBLE_RANGE: i32 = 103;
const ATTR_WARP_SCRAMBLE_STRENGTH: i32 = 105;
/// Jam strengths: gravimetric, ladar, magnetometric, radar.
const ATTR_JAM_STRENGTHS: [i32; 4] = [238, 239, 240, 241];
const ATTR_MAX_TARGET_RANGE_MULTIPLIER: i32 = 237;
const ATTR_SCAN_RESOLUTION_MULTIPLIER: i32 = 565;

/// A tower's sentry settings (posMgr Get/SetTowerSentrySettings).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Sentry {
    pub standing: Option<f64>,
    pub status: Option<f64>,
    pub standing_drop: bool,
    pub status_drop: bool,
    pub aggression: bool,
    pub war: bool,
}

/// What a defence does each cycle.
#[derive(Debug, Clone, PartialEq)]
pub enum Weapon {
    /// Guns: the battery's damage multiplier and gun.
    Gun { multiplier: f64, gun: Gun },
    Missile,
    /// Web, scrambler, jammer or damper, out to an optimal and falloff.
    Ew { action: Action, optimal: f64, falloff: f64 },
}

/// The round loaded in a gun or missile battery's bay.
#[derive(Debug, Clone, PartialEq)]
pub struct Ammo {
    pub item_id: i64,
    pub type_id: i64,
    pub quantity: i64,
    pub damage: Damage,
    /// Crystals are not used up.
    pub lasting: bool,
    /// For missiles: flight time, flight range, explosion radius and speed.
    pub flight_ms: i64,
    pub range: f64,
    pub cloud: f64,
    pub aoe_velocity: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Guard {
    pub weapon: Weapon,
    pub search: f64,
    pub switch_ms: i64,
    pub cycle_ms: i64,
    pub ammo: Option<Ammo>,
    pub target: Option<i64>,
    /// When it may next act: the end of acquiring a target, or of a cycle.
    pub ready_ms: i64,
    /// Whether its EW holds on the target this cycle.
    pub landed: bool,
}

/// The guard profile of a defence type, if it is one.
pub fn profile(group_id: i64, a: &HashMap<i32, f64>) -> Option<Guard> {
    let v = |id: i32| a.get(&id).copied().unwrap_or(0.0);
    let ms = |id: i32, default: f64| (if v(id) > 0.0 { v(id) } else { default }) as i64;
    let gun = || Gun {
        optimal: v(attr::MAX_RANGE),
        falloff: v(attr::FALLOFF),
        tracking: v(attr::TRACKING_SPEED),
        optimal_sig: v(attr::OPTIMAL_SIG_RADIUS),
    };
    let (weapon, cycle_ms) = match group_id {
        GROUP_PROJECTILE_SENTRY | GROUP_LASER_SENTRY | GROUP_HYBRID_SENTRY => {
            (Weapon::Gun { multiplier: v(attr::DAMAGE_MULTIPLIER).max(1.0), gun: gun() }, ms(ATTR_SPEED, 10_000.0))
        }
        GROUP_MISSILE_SENTRY => (Weapon::Missile, ms(ATTR_MISSILE_LAUNCH_DURATION, 10_000.0)),
        GROUP_WEB_BATTERY => {
            let action = Action::Web((1.0 + v(ATTR_SPEED_FACTOR) / 100.0).clamp(0.01, 1.0));
            (Weapon::Ew { action, optimal: v(attr::MAX_RANGE), falloff: 0.0 }, ms(ATTR_DURATION, 10_000.0))
        }
        GROUP_SCRAMBLE_BATTERY => {
            let action = Action::Scramble(v(ATTR_WARP_SCRAMBLE_STRENGTH).max(1.0));
            (Weapon::Ew { action, optimal: v(ATTR_WARP_SCRAMBLE_RANGE), falloff: 0.0 }, ms(ATTR_DURATION, 10_000.0))
        }
        GROUP_EW_BATTERY => {
            let action = Action::Jam(ATTR_JAM_STRENGTHS.map(v));
            (Weapon::Ew { action, optimal: v(attr::MAX_RANGE), falloff: v(attr::FALLOFF) }, ms(ATTR_DURATION, 10_000.0))
        }
        GROUP_DAMPENING_BATTERY => {
            let multiplier = |id: i32| if v(id) > 0.0 { v(id) } else { 1.0 };
            let action = Action::Dampen { range: multiplier(ATTR_MAX_TARGET_RANGE_MULTIPLIER), resolution: multiplier(ATTR_SCAN_RESOLUTION_MULTIPLIER) };
            (Weapon::Ew { action, optimal: v(attr::MAX_RANGE), falloff: v(attr::FALLOFF) }, ms(ATTR_DURATION, 10_000.0))
        }
        _ => return None,
    };
    Some(Guard {
        weapon,
        search: v(ATTR_PROXIMITY_RANGE),
        switch_ms: ms(ATTR_TARGET_SWITCH_DELAY, 0.0),
        cycle_ms: cycle_ms.max(1_000),
        ammo: None,
        target: None,
        ready_ms: 0,
        landed: false,
    })
}

/// The attributes guard profiles read, for pos::load.
pub fn profile_attributes() -> Vec<i32> {
    let mut a = vec![
        attr::MAX_RANGE, attr::FALLOFF, attr::TRACKING_SPEED, attr::OPTIMAL_SIG_RADIUS, attr::DAMAGE_MULTIPLIER,
        ATTR_SPEED, ATTR_PROXIMITY_RANGE, ATTR_TARGET_SWITCH_DELAY, ATTR_MISSILE_LAUNCH_DURATION, ATTR_DURATION,
        ATTR_SPEED_FACTOR, ATTR_WARP_SCRAMBLE_RANGE, ATTR_WARP_SCRAMBLE_STRENGTH,
        ATTR_MAX_TARGET_RANGE_MULTIPLIER, ATTR_SCAN_RESOLUTION_MULTIPLIER,
    ];
    a.extend(ATTR_JAM_STRENGTHS);
    a
}

/// What each gun and missile battery has in its bay: the first charge, by
/// item ID.
pub async fn ammo(db: &impl GenericClient, batteries: &[i64]) -> Result<HashMap<i64, Ammo>, tokio_postgres::Error> {
    let ids: Vec<i32> = batteries.iter().map(|i| *i as i32).collect();
    let rows = db
        .query(
            r#"SELECT DISTINCT ON (e."locationID") e."locationID", e."itemID", e."typeID", e."quantity", t."groupID"
                 FROM entity e
                 JOIN "invTypes" t ON t."typeID" = e."typeID"
                 JOIN "invGroups" g ON g."groupID" = t."groupID"
                WHERE e."locationID" = ANY($1) AND g."categoryID" = $2 AND e."quantity" > 0
                ORDER BY e."locationID", e."itemID""#,
            &[&ids, &CATEGORY_CHARGE],
        )
        .await?;
    let types: Vec<i32> = rows.iter().filter_map(|r| int(r, 2)).map(|t| t as i32).collect();
    let mut wanted = attr::DAMAGE_TYPES.to_vec();
    wanted.extend([attr::EXPLOSION_DELAY, attr::MAX_VELOCITY, attr::AOE_CLOUD_SIZE, attr::AOE_VELOCITY]);
    let attrs = db
        .query(
            r#"SELECT "typeID", "attributeID", COALESCE("valueFloat", "valueInt"::float8)
                 FROM "dgmTypeAttributes" WHERE "typeID" = ANY($1) AND "attributeID" = ANY($2)"#,
            &[&types, &wanted],
        )
        .await?;
    let mut by_type: HashMap<i64, HashMap<i32, f64>> = HashMap::new();
    for r in attrs {
        if let (Some(t), Some(a), Some(v)) = (int(&r, 0), int(&r, 1), float(&r, 2)) {
            by_type.entry(t).or_default().insert(a as i32, v);
        }
    }
    Ok(rows
        .iter()
        .filter_map(|r| {
            let type_id = int(r, 2)?;
            let a = by_type.get(&type_id);
            let v = |id: i32| a.and_then(|a| a.get(&id)).copied().unwrap_or(0.0);
            let flight_ms = v(attr::EXPLOSION_DELAY) as i64;
            Some((
                int(r, 0)?,
                Ammo {
                    item_id: int(r, 1)?,
                    type_id,
                    quantity: int(r, 3).unwrap_or(0),
                    damage: attr::DAMAGE_TYPES.map(v),
                    lasting: int(r, 4) == Some(GROUP_FREQUENCY_CRYSTAL),
                    flight_ms,
                    range: v(attr::MAX_VELOCITY) * flight_ms as f64 / 1_000.0,
                    cloud: v(attr::AOE_CLOUD_SIZE),
                    aoe_velocity: v(attr::AOE_VELOCITY),
                },
            ))
        })
        .collect())
}

/// Reloads what every loaded park's batteries have in their bays (pilots
/// fill them through the ordinary item moves). Run by the heartbeat.
pub async fn reload(world: &crate::world::World, db: &impl GenericClient) {
    let batteries: Vec<i64> = {
        let w = world.lock();
        w.parks
            .values()
            .flat_map(|p| p.structures.values())
            .filter(|s| s.guard.as_ref().is_some_and(|g| !matches!(g.weapon, Weapon::Ew { .. })))
            .map(|s| s.id)
            .collect()
    };
    if batteries.is_empty() {
        return;
    }
    let loaded = match ammo(db, &batteries).await {
        Ok(a) => a,
        Err(e) => {
            error!("reading the batteries' ammunition: {}", crate::logging::chain(&e));
            return;
        }
    };
    let mut w = world.lock();
    for park in w.parks.values_mut() {
        for s in park.structures.values_mut() {
            if let Some(g) = s.guard.as_mut().filter(|g| !matches!(g.weapon, Weapon::Ew { .. })) {
                g.ammo = loaded.get(&s.id).cloned();
            }
        }
    }
}

// ---- standings ---------------------------------------------------------------------------

/// (from, to) -> standing, from ownerStandings; reloaded by the heartbeat.
static STANDINGS: RwLock<Option<HashMap<(i64, i64), f64>>> = RwLock::new(None);

pub async fn refresh_standings(db: &impl GenericClient) {
    let sql = r#"SELECT "fromID", "toID", "standing" FROM "ownerStandings""#;
    match db.query(sql, &[]).await {
        Ok(rows) => {
            let map = rows.iter().filter_map(|r| Some(((int(r, 0)?, int(r, 1)?), float(r, 2)?))).collect();
            if let Ok(mut s) = STANDINGS.write() {
                *s = Some(map);
            }
        }
        Err(e) => error!("reading owner standings: {}", crate::logging::chain(&e)),
    }
}

/// The owner's standing toward a pilot: its corporation's, else its
/// alliance's; toward the pilot, else its corporation, else its alliance.
fn standing(corp: i64, alliance: Option<i64>, pilot: &Pilot) -> f64 {
    let Ok(guard) = STANDINGS.read() else { return 0.0 };
    let Some(map) = guard.as_ref() else { return 0.0 };
    let froms = std::iter::once(corp).chain(alliance);
    let tos: Vec<i64> = std::iter::once(pilot.char_id).chain([pilot.corp_id]).chain(pilot.alliance_id).collect();
    for from in froms {
        for to in &tos {
            if let Some(s) = map.get(&(from, *to)) {
                return *s;
            }
        }
    }
    0.0
}

// ---- engaging ----------------------------------------------------------------------------

/// The pilot behind a ship (its own, or a drone's owner) as the defence
/// judges it; None for what it leaves alone.
fn judged(ship: &Ship) -> Option<(i64, &Pilot)> {
    if ship.npc.is_some() || ship.in_warp() {
        return None;
    }
    match &ship.drone {
        Some(d) => Some((d.owner, &ship.pilot)),
        None if ship.is_player() => Some((ship.pilot.char_id, &ship.pilot)),
        None => None,
    }
}

/// Whether the tower's settings make this ship a target of `s`'s.
fn hostile(park: &Park, s: &Structure, sentry: &Sentry, ship: &Ship, now: i64) -> bool {
    let Some((char_id, pilot)) = judged(ship) else { return false };
    let friend = char_id == s.owner_id
        || pilot.corp_id == s.corp_id
        || (s.alliance_id.is_some() && pilot.alliance_id == s.alliance_id);
    if friend {
        return false;
    }
    let standing = || standing(s.corp_id, s.alliance_id, pilot);
    let owner_side = s.alliance_id.unwrap_or(s.corp_id);
    let attacked_us = || {
        park.watch.aggressed(char_id, s.corp_id, now)
            || park.ships.values().any(|v| v.pilot.corp_id == s.corp_id && v.is_player() && park.watch.aggressed(char_id, v.pilot.char_id, now))
    };
    sentry.war && crate::svc::war::sides_at_war(owner_side, crate::svc::war::side(pilot))
        || sentry.aggression && attacked_us()
        || sentry.status.is_some_and(|x| pilot.security < x)
        || sentry.status_drop && park.watch.is_criminal(char_id, now)
        || sentry.standing.is_some_and(|x| standing() < x)
        || sentry.standing_drop && standing() < 0.0
}

/// One pulse of every defence here.
pub fn step(park: &mut Park, now: i64, out: &mut Out) {
    let filetime = crate::bulkdata::filetime_now();
    let ids: Vec<i64> = park.structures.values().filter(|s| s.guard.is_some()).map(|s| s.id).collect();
    for id in ids {
        let Some(s) = park.structures.get(&id) else { continue };
        let tower = s.tower.and_then(|t| park.structures.get(&t));
        let armed = s.fully_online(filetime) && tower.is_some_and(|t| t.fully_online(filetime));
        let sentry = tower.map(|t| t.sentry).unwrap_or_default();
        let Some(g) = s.guard.as_ref() else { continue };

        // Keep the target while it stays hostile and in reach; else the nearest that is.
        let in_reach = |ship: &Ship| (ship.pos - s.pos).len() - ship.stats.radius - s.radius <= g.search;
        let keeps = armed
            && g.target.and_then(|t| park.ships.get(&t)).is_some_and(|ship| in_reach(ship) && hostile(park, s, &sentry, ship, now));
        let target = if keeps {
            g.target
        } else if armed {
            park.ships
                .values()
                .filter(|ship| in_reach(ship) && hostile(park, s, &sentry, ship, now))
                .min_by(|a, b| (a.pos - s.pos).len_sq().total_cmp(&(b.pos - s.pos).len_sq()))
                .map(|ship| ship.id)
        } else {
            None
        };
        let fresh = target.is_some() && target != g.target;
        let ready = !fresh && target.is_some() && now >= g.ready_ms;
        let Some(g) = park.structures.get_mut(&id).and_then(|s| s.guard.as_mut()) else { continue };
        g.target = target;
        if fresh {
            g.ready_ms = now + g.switch_ms;
            g.landed = false;
            debug!("defence {id} engages {target:?}");
        }
        if target.is_none() {
            g.landed = false;
        }
        if ready {
            g.ready_ms = now + g.cycle_ms;
            act(park, id, target.unwrap_or(0), now, out);
        }
    }
}

/// One cycle of a defence at its target.
fn act(park: &mut Park, id: i64, target: i64, now: i64, out: &mut Out) {
    let Some(s) = park.structures.get(&id) else { return };
    let Some(g) = s.guard.clone() else { return };
    let Some(ship) = park.ships.get(&target) else { return };
    let distance = ((ship.pos - s.pos).len() - ship.stats.radius - s.radius).max(0.0);
    let by = Attacker { ship_id: id, char_id: 0, client_id: None, ship_type: s.type_id, weapon_type: s.type_id };
    let name = s.name.clone();
    let (spos, tpos, tvel) = (s.pos, ship.pos, ship.vel);
    let signature = ship.fight.as_ref().map_or(ship.stats.radius, |f| f.sig());
    let sensors = ship.fight.as_ref().map_or([0.0; 4], |f| f.sensors);
    match g.weapon {
        Weapon::Gun { multiplier, gun } => {
            let Some(ammo) = g.ammo.as_ref().filter(|a| a.quantity > 0) else { return };
            let angular = combat::angular_velocity(spos, Vec3::ZERO, tpos, tvel);
            let chance = combat::hit_chance(&gun, distance, angular, signature);
            let shot = combat::turret_shot(chance, park.rng.unit(), park.rng.unit());
            let damage = ammo.damage.map(|d| d * multiplier * shot.multiplier());
            let lasting = ammo.lasting;
            if !lasting {
                use_round(park, id, out);
            }
            crate::fight::hit_by(park, by, target, damage, shot, &name, now, out);
        }
        Weapon::Missile => {
            let Some(ammo) = g.ammo.clone().filter(|a| a.quantity > 0) else { return };
            use_round(park, id, out);
            let speed = if ammo.flight_ms > 0 { ammo.range / ammo.flight_ms as f64 } else { 1.0 };
            park.missiles.push(Missile {
                attacker: by,
                target,
                impact_ms: now + (distance / speed.max(1e-3)).min(ammo.flight_ms.max(0) as f64) as i64,
                damage: ammo.damage,
                cloud: ammo.cloud,
                aoe_velocity: ammo.aoe_velocity,
                reaches: distance <= ammo.range,
                weapon: name,
            });
        }
        Weapon::Ew { action, optimal, falloff } => {
            let mut landed = park.rng.unit() < combat::falloff_chance(distance, optimal, falloff);
            if let Action::Jam(strength) = action {
                landed = landed && park.rng.unit() < combat::jam_chance(strength, sensors);
            }
            if let Some(g) = park.structures.get_mut(&id).and_then(|s| s.guard.as_mut()) {
                g.landed = landed;
            }
        }
    }
}

/// A round leaves a battery's bay; the heartbeat saves it.
fn use_round(park: &mut Park, id: i64, out: &mut Out) {
    let Some(ammo) = park.structures.get_mut(&id).and_then(|s| s.guard.as_mut()).and_then(|g| g.ammo.as_mut()) else { return };
    ammo.quantity -= 1;
    out.jobs.push(Job::StructureAmmo { item_id: ammo.item_id, quantity: ammo.quantity });
}

/// The EW defences are holding on ships: (target, what it does), for afflict.
pub fn holding(park: &Park) -> Vec<(i64, Action)> {
    park.structures
        .values()
        .filter_map(|s| s.guard.as_ref())
        .filter(|g| g.landed)
        .filter_map(|g| match (&g.weapon, g.target) {
            (Weapon::Ew { action, .. }, Some(t)) => Some((t, action.clone())),
            _ => None,
        })
        .collect()
}

/// A battery's round count, after a shot.
pub async fn save_ammo(db: &impl GenericClient, item_id: i64, quantity: i64) {
    let result = if quantity <= 0 {
        db.execute(r#"DELETE FROM entity WHERE "itemID" = $1"#, &[&(item_id as i32)]).await
    } else {
        db.execute(r#"UPDATE entity SET "quantity" = $2 WHERE "itemID" = $1"#, &[&(item_id as i32), &(quantity as i32)]).await
    };
    if let Err(e) = result {
        error!("saving the rounds of {item_id}: {}", crate::logging::chain(&e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batteries_get_the_profile_of_their_group() {
        let mut a = HashMap::new();
        a.insert(ATTR_SPEED_FACTOR, -75.0);
        a.insert(attr::MAX_RANGE, 150_000.0);
        a.insert(ATTR_DURATION, 10_000.0);
        let g = profile(GROUP_WEB_BATTERY, &a).expect("a web battery");
        assert_eq!(g.weapon, Weapon::Ew { action: Action::Web(0.25), optimal: 150_000.0, falloff: 0.0 });
        assert_eq!(g.cycle_ms, 10_000);
        assert!(profile(365, &a).is_none(), "a tower is not a defence");
    }
}
