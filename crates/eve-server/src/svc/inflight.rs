//! dogmaIM in space: locking targets and running modules (targetMgr.py,
//! godma.py), and what a ship brings to a fight, as the dogma engine computes
//! it from the ship, its fitting and its pilot's skills.
//!
//! A lock takes 40000 / (scanResolution * asinh(signature)^2) seconds, so
//! AddTarget answers "pending" and the heartbeat sends OnTarget("add") when it
//! completes. Activate builds the module's cycle from its computed attributes
//! and those of its charge; the heartbeat runs it from there.

use std::collections::{BTreeSet, HashSet};

use eve_marshal::Value;
use tokio_postgres::GenericClient;
use tracing::{debug, error, info};

use crate::ballpark::{Pilot, Ship, ShipStats};
use crate::combat::{self, attr, Action, Active, Ammo, Charged, Fighter, Gun, Lock, Vitals, REPEAT_FOREVER};
use crate::dogma::{category, Catalog, Loadout, State};
use crate::fight;
use crate::services::{Call, Ctx, Outcome};
use crate::svc::space::{self, entry};

/// Effect categories (dgmEffects.effectCategory).
const TARGETED: i16 = 2;
/// Effects whose cycle is hard-coded here; any other active effect is
/// [`Action::Passive`]: dogma applies its modifiers while it runs.
mod effect {
    pub const SHIELD_BOOSTING: i64 = 4;
    pub const SPEED_BOOST: i64 = 7;
    pub const TARGET_ATTACK: i64 = 10;
    pub const MINING: i64 = 17;
    pub const STRUCTURE_REPAIR: i64 = 26;
    pub const ARMOR_REPAIR: i64 = 27;
    pub const PROJECTILE_FIRED: i64 = 34;
    pub const POWER_BOOSTER: i64 = 48;
    pub const MINING_LASER: i64 = 67;
    pub const USE_MISSILES: i64 = 101;
    pub const SPEED_BOOST_MASS_ADDITION: i64 = 1253;
    pub const SPEED_BOOST_MASS_SIG_RAD: i64 = 1254;
    pub const SPEED_BOOST_MASS_SIG_VELOCITY: i64 = 1417;
    pub const SHIELD_TRANSFER: i64 = 18;
    pub const MODIFY_TARGET_SPEED: i64 = 29;
    pub const ENERGY_TRANSFER: i64 = 31;
    pub const ENERGY_VAMPIRE: i64 = 32;
    pub const ENERGY_DESTABILIZATION: i64 = 36;
    pub const WARP_SCRAMBLE: i64 = 39;
    pub const DECREASE_TARGET_SPEED: i64 = 586;
    pub const TARGET_ARMOR_REPAIR: i64 = 592;
    pub const DISRUPT: i64 = 609;
    pub const DAMPEN: i64 = 837;
    pub const DISRUPT_TEST: i64 = 1274;
    pub const DAMPEN_TEST: i64 = 1354;
    pub const JAM: i64 = 1358;
    pub const TARGET_PAINT_2: i64 = 1507;
    pub const TARGET_PAINT: i64 = 1549;
    pub const ENERGY_DESTABILIZATION_NEW: i64 = 2303;
    pub const ENERGY_NOSFERATU: i64 = 2304;

    pub const PROPULSION: [i32; 4] =
        [SPEED_BOOST as i32, SPEED_BOOST_MASS_ADDITION as i32, SPEED_BOOST_MASS_SIG_RAD as i32, SPEED_BOOST_MASS_SIG_VELOCITY as i32];
}
/// Frequency and mining crystals last; everything else is used up.
const REUSABLE_CHARGE_GROUPS: [i64; 2] = [86, 482];

// ---- what a ship is ----------------------------------------------------------------------

/// A ship as it flies and fights.
pub struct Profile {
    pub stats: ShipStats,
    pub pilot: Pilot,
    pub fighter: Fighter,
}

/// Builds `ship_id`'s profile for `char_id`, with the effects of the
/// `active` modules applied. Its hit points come from what is stored on the
/// ship: armour and structure damage, and the shield and capacitor as they
/// were when it was last in space (full if never).
pub async fn profile(
    db: &impl GenericClient,
    catalog: &Catalog,
    char_id: i64,
    ship_id: i64,
    active: &BTreeSet<i64>,
    now: i64,
) -> Result<Option<Profile>, tokio_postgres::Error> {
    let running: HashSet<i64> = active.iter().copied().collect();
    let loadout = Loadout::load(db, catalog, char_id, Some(ship_id), &running).await?;
    let (Some(ship), character) = (loadout.ship(), loadout.character()) else { return Ok(None) };
    let Some(r) = db
        .query_opt(
            r#"SELECT e."typeID", t."groupID", COALESCE(NULLIF(e."itemName", ''), t."typeName", ''),
                      COALESCE(t."radius", 0), COALESCE(t."mass", 0),
                      c."corporationID", co."allianceID",
                      COALESCE(c."securityRating", 0), COALESCE(c."bounty", 0)
                 FROM entity e
                 JOIN "invTypes" t ON t."typeID" = e."typeID"
                 LEFT JOIN "character_" c ON c."characterID" = $2
                 LEFT JOIN corporation co ON co."corporationID" = c."corporationID"
                WHERE e."itemID" = $1"#,
            &[&(ship_id as i32), &(char_id as i32)],
        )
        .await?
    else {
        return Ok(None);
    };
    let stored: Vec<(i32, f64)> = db
        .query(
            r#"SELECT "attributeID", coalesce("valueFloat", "valueInt"::float8) FROM entity_attributes
                WHERE "itemID" = $1 AND "attributeID" = ANY($2)"#,
            &[&(ship_id as i32), &vec![attr::SHIELD_CHARGE, attr::CHARGE]],
        )
        .await?
        .iter()
        .filter_map(|r| Some((r.get(0), r.get::<_, Option<f64>>(1)?)))
        .collect();

    let v = |a: i32| loadout.value(ship, a);
    let mut mass = r.get::<_, f64>(4);
    let mut max_velocity = v(attr::MAX_VELOCITY);
    let mut signature = v(attr::SIGNATURE_RADIUS);
    // Afterburners and microwarpdrives: thrust over mass, as 2.10 hard-coded it.
    for (i, p) in loadout.pieces.iter().enumerate() {
        if p.state != State::Active || !effect::PROPULSION.iter().any(|e| p.has_effect(*e)) {
            continue;
        }
        mass += loadout.value(i, attr::MASS_ADDITION);
        let thrust = loadout.value(i, attr::SPEED_BOOST_FACTOR);
        let factor = loadout.value(i, attr::SPEED_FACTOR) / 100.0;
        if mass > 0.0 {
            max_velocity *= 1.0 + factor * thrust / mass;
        }
        signature *= 1.0 + loadout.value(i, attr::SIGNATURE_RADIUS_BONUS) / 100.0;
    }
    let stats = ShipStats::new(r.get::<_, f64>(3), mass, v(attr::AGILITY), max_velocity, v(600));
    let pilot = Pilot {
        type_id: r.get::<_, i32>(0) as i64,
        group_id: r.get::<_, i32>(1) as i64,
        name: r.get(2),
        char_id,
        corp_id: r.get::<_, Option<i32>>(5).map_or(0, i64::from),
        alliance_id: r.get::<_, Option<i32>>(6).map(i64::from).filter(|a| *a != 0),
        security: r.get(7),
        bounty: r.get(8),
    };

    let mut shield = Charged::full(v(attr::SHIELD_CAPACITY), v(attr::SHIELD_RECHARGE_RATE), now);
    let mut capacitor = Charged::full(v(attr::CAPACITOR_CAPACITY), v(attr::RECHARGE_RATE), now);
    for (a, value) in stored {
        match a {
            attr::SHIELD_CHARGE => shield.set(now, value),
            attr::CHARGE => capacitor.set(now, value),
            _ => {}
        }
    }
    let locks_ship = v(attr::MAX_LOCKED_TARGETS);
    let locks_skill = character.map_or(locks_ship, |c| loadout.value(c, attr::MAX_LOCKED_TARGETS));
    let max_locked = if locks_skill > 0.0 { locks_ship.min(locks_skill) } else { locks_ship };
    let vitals = Vitals {
        shield,
        armor_max: v(attr::ARMOR_HP),
        armor_damage: v(attr::ARMOR_DAMAGE).max(0.0),
        hull_max: v(attr::HP),
        hull_damage: v(attr::DAMAGE).max(0.0),
        capacitor,
        resonance: [attr::SHIELD_RESONANCE.map(v), attr::ARMOR_RESONANCE.map(v), attr::HULL_RESONANCE.map(v)],
    };
    let mut fighter = Fighter::new(
        vitals,
        signature.max(1.0),
        v(attr::SCAN_RESOLUTION),
        v(attr::MAX_TARGET_RANGE),
        max_locked.max(1.0) as usize,
    );
    fighter.applied = active.clone();
    fighter.top_speed = stats.max_velocity;
    fighter.sensors = attr::SENSOR_STRENGTHS.map(v);
    Ok(Some(Profile { stats, pilot, fighter }))
}

/// A ship entering space: its profile, made into a park ship.
pub async fn ship_for_space(ctx: &Ctx<'_>, char_id: i64, ship_id: i64) -> Option<Ship> {
    let now = combat::now_ms();
    match profile(ctx.db, ctx.dogma, char_id, ship_id, &BTreeSet::new(), now).await {
        Ok(Some(p)) => {
            let mut ship = Ship::new(ship_id, ctx.client_id, p.stats, p.pilot, Default::default(), Default::default());
            ship.fight = Some(p.fighter);
            Some(ship)
        }
        Ok(None) => None,
        Err(e) => {
            error!("loading ship {ship_id}: {}", crate::logging::chain(&e));
            None
        }
    }
}

/// Moves a profile's numbers onto a ship already in space, keeping its hit
/// points (as fractions where a maximum changed), locks and modules.
pub fn apply_profile(ship: &mut Ship, p: Profile, now: i64) {
    ship.stats = p.stats;
    ship.pilot = p.pilot;
    let Some(fight) = ship.fight.as_mut() else {
        ship.fight = Some(p.fighter);
        return;
    };
    let new = p.fighter;
    fight.vitals.shield.rescale(now, new.vitals.shield.max, new.vitals.shield.recharge_ms);
    fight.vitals.capacitor.rescale(now, new.vitals.capacitor.max, new.vitals.capacitor.recharge_ms);
    fight.vitals.armor_max = new.vitals.armor_max;
    fight.vitals.hull_max = new.vitals.hull_max;
    fight.vitals.resonance = new.vitals.resonance;
    fight.signature = new.signature;
    fight.scan_resolution = new.scan_resolution;
    fight.max_target_range = new.max_target_range;
    fight.max_locked = new.max_locked;
    fight.applied = new.applied;
    fight.top_speed = new.top_speed;
    fight.sensors = new.sensors;
    // Webs still hold: the next pulse slows it again.
    ship.stats.max_velocity = new.top_speed * fight.afflictions.speed;
}

/// Stores what a ship leaving space has left: damage always; the shield and
/// capacitor only while it stays out there (docked, they refill).
pub async fn save_vitals(db: &impl GenericClient, ship_id: i64, fighter: &Fighter, docked: bool) {
    let now = combat::now_ms();
    let v = &fighter.vitals;
    let mut set = vec![(attr::ARMOR_DAMAGE, v.armor_damage), (attr::DAMAGE, v.hull_damage)];
    let mut clear = Vec::new();
    if docked {
        clear.extend([attr::SHIELD_CHARGE, attr::CHARGE]);
    } else {
        set.extend([(attr::SHIELD_CHARGE, v.shield.at(now)), (attr::CHARGE, v.capacitor.at(now))]);
    }
    for (a, value) in set {
        let result = if value <= 0.0 && matches!(a, attr::ARMOR_DAMAGE | attr::DAMAGE) {
            db.execute(r#"DELETE FROM entity_attributes WHERE "itemID" = $1 AND "attributeID" = $2"#, &[&(ship_id as i32), &a]).await
        } else {
            db.execute(
                r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt","valueFloat")
                   VALUES ($1, $2, NULL, $3)
                   ON CONFLICT ("itemID","attributeID") DO UPDATE SET "valueInt" = NULL, "valueFloat" = $3"#,
                &[&(ship_id as i32), &a, &value],
            )
            .await
        };
        if let Err(e) = result {
            error!("saving attribute {a} of ship {ship_id}: {}", crate::logging::chain(&e));
        }
    }
    if !clear.is_empty() {
        if let Err(e) = db
            .execute(r#"DELETE FROM entity_attributes WHERE "itemID" = $1 AND "attributeID" = ANY($2)"#, &[&(ship_id as i32), &clear])
            .await
        {
            error!("clearing charges of ship {ship_id}: {}", crate::logging::chain(&e));
        }
    }
}

// ---- targeting --------------------------------------------------------------------------

pub async fn dogma_im(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let outcome = match call.method.as_str() {
        "AddTarget" => add_target(ctx, call.arg_id(0)),
        "RemoveTarget" => remove_target(ctx, call.arg_id(0)),
        "ClearTargets" => clear_targets(ctx),
        "GetTargets" => Outcome::Value(Value::List(with_fight(ctx, |_, f, _| f.locked().map(Value::Int).collect()).unwrap_or_default())),
        "GetTargeters" => Outcome::Value(Value::List(targeters(ctx).into_iter().map(Value::Int).collect())),
        "Activate" => activate(ctx, call).await,
        "Deactivate" => deactivate(ctx, call).await,
        _ => return None,
    };
    Some(outcome)
}

/// Runs `f` on the caller's ship in space, under the world lock.
fn with_fight<T>(ctx: &Ctx<'_>, f: impl FnOnce(&mut crate::ballpark::Park, &mut Fighter, i64) -> T) -> Option<T> {
    let (system, ship_id) = (ctx.session.solar_system_id?, ctx.session.ship_id?);
    let mut world = ctx.world.lock();
    let park = world.parks.get_mut(&system)?;
    let mut fighter = park.ships.get_mut(&ship_id)?.fight.take()?;
    let result = f(park, &mut fighter, ship_id);
    if let Some(ship) = park.ships.get_mut(&ship_id) {
        ship.fight = Some(fighter);
    }
    Some(result)
}

fn user_error(msg: &str, args: Vec<(&str, Value)>) -> Outcome {
    Outcome::UserError { msg: msg.into(), args: args.into_iter().map(|(k, v)| (k.into(), v)).collect() }
}

fn add_target(ctx: &Ctx<'_>, target: i64) -> Outcome {
    let now = combat::now_ms();
    let result = with_fight(ctx, |park, fight, me| -> Result<Value, Outcome> {
        let locked: Vec<Value> = fight.locked().map(Value::Int).collect();
        if fight.locks.contains_key(&target) {
            return Ok(Value::Tuple(vec![Value::Int(i64::from(!fight.is_locked(target))), Value::List(locked)]));
        }
        if target == me {
            return Err(Outcome::custom("You cannot target yourself."));
        }
        let name = park
            .ships
            .get(&target)
            .map(|s| s.pilot.name.clone())
            .or_else(|| park.rocks.get(&target).map(|r| r.name.clone()))
            .or_else(|| park.celestial(target).map(|c| c.name.clone()));
        let Some(name) = name else {
            return Err(user_error("TargetCantValidate", Vec::new()));
        };
        if park.ships.get(&me).is_some_and(|s| s.warping()) {
            return Err(Outcome::custom("You cannot lock targets while warping."));
        }
        if fight.afflictions.jammed {
            return Err(Outcome::custom("Your targeting systems are being jammed."));
        }
        if fight.locks.len() >= fight.max_locked {
            return Err(user_error("TargetingSystemsAlreadyFullyUtilized", vec![("limit", Value::Int(fight.max_locked as i64))]));
        }
        let distance = park.surface_distance(me, target).unwrap_or(f64::MAX);
        if distance > fight.lock_range() {
            return Err(user_error("TargetOutOfRangeFar", vec![("targetname", Value::text(name))]));
        }
        let signature = match park.ships.get(&target) {
            Some(s) => s.fight.as_ref().map_or(100.0, |f| f.sig()),
            None => park.ball(target).map_or(100.0, |(_, r)| r),
        };
        let seconds = combat::lock_seconds(fight.scan_res(), signature);
        fight.locks.insert(target, Lock::Pending(now + (seconds * 1_000.0) as i64));
        debug!("locking {target} ({name}) from {distance:.0} m: {seconds:.1}s");
        Ok(Value::Tuple(vec![Value::Int(1), Value::List(locked)]))
    });
    match result {
        Some(Ok(v)) => Outcome::Value(v),
        Some(Err(refusal)) => refusal,
        None => Outcome::custom("You are not in space."),
    }
}

fn remove_target(ctx: &Ctx<'_>, target: i64) -> Outcome {
    unlock(ctx, Some(target));
    Outcome::Value(Value::None)
}

fn clear_targets(ctx: &Ctx<'_>) -> Outcome {
    unlock(ctx, None);
    Outcome::Value(Value::None)
}

/// Drops one lock, or all of them, and tells both sides.
fn unlock(ctx: &Ctx<'_>, only: Option<i64>) {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else { return };
    let mut world = ctx.world.lock();
    let Some(park) = world.parks.get_mut(&system) else { return };
    let Some(fight) = park.ships.get_mut(&ship_id).and_then(|s| s.fight.as_mut()) else { return };
    let dropped: Vec<(i64, Lock)> = match only {
        Some(t) => fight.locks.remove(&t).map(|l| (t, l)).into_iter().collect(),
        None => std::mem::take(&mut fight.locks).into_iter().collect(),
    };
    let mut out = crate::heartbeat::Out::default();
    for (t, lock) in &dropped {
        if *lock == Lock::Locked {
            fight::tell_target_lost(park, ship_id, *t, &mut out);
        }
    }
    let event = match only {
        Some(t) => Value::Tuple(vec![Value::bytes("lost"), Value::Int(t), Value::None]),
        None => Value::Tuple(vec![Value::bytes("clear")]),
    };
    out.note(ctx.client_id, "OnTarget", event);
    out.deliver(&world);
}

/// Ships that have the caller locked.
fn targeters(ctx: &Ctx<'_>) -> Vec<i64> {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else { return Vec::new() };
    let world = ctx.world.lock();
    let Some(park) = world.parks.get(&system) else { return Vec::new() };
    park.ships
        .values()
        .filter(|s| s.fight.as_ref().is_some_and(|f| f.is_locked(ship_id)))
        .map(|s| s.id)
        .collect()
}

// ---- modules ------------------------------------------------------------------------------

/// What dgmEffects says about an effect.
struct EffectRow {
    id: i64,
    name: String,
    category: i16,
    guid: String,
    offensive: bool,
    duration_attr: Option<i32>,
    discharge_attr: Option<i32>,
    range_attr: Option<i32>,
    falloff_attr: Option<i32>,
}

async fn effect_by_name(ctx: &Ctx<'_>, name: &str) -> Option<EffectRow> {
    let sql = r#"SELECT "effectID", "effectName", COALESCE("effectCategory", 0)::int2, COALESCE(guid, ''),
                        COALESCE("isOffensive", 0)::int, "durationAttributeID", "dischargeAttributeID",
                        "rangeAttributeID", "falloffAttributeID"
                   FROM "dgmEffects" WHERE "effectName" = $1"#;
    let r = ctx.query_one(sql, &[&name]).await?;
    Some(EffectRow {
        id: r.get::<_, i32>(0) as i64,
        name: r.get(1),
        category: r.get(2),
        guid: r.get(3),
        offensive: r.get::<_, i32>(4) != 0,
        duration_attr: r.get::<_, Option<i32>>(5),
        discharge_attr: r.get::<_, Option<i32>>(6),
        range_attr: r.get::<_, Option<i32>>(7),
        falloff_attr: r.get::<_, Option<i32>>(8),
    })
}

fn effect_name(v: Option<&Value>) -> Option<String> {
    v.and_then(|v| v.as_string())
}

/// Activate(itemID, effectName, target, repeat).
async fn activate(ctx: &Ctx<'_>, call: &Call) -> Outcome {
    let item_id = call.arg_id(0);
    let Some(name) = effect_name(call.args.get(1)) else {
        return Outcome::custom("Which effect should that module run?");
    };
    if name == "online" {
        return crate::svc::fitting::set_online(ctx, item_id, true).await;
    }
    let target = call.args.get(2).and_then(|v| v.as_i64());
    let repeat = call.args.get(3).and_then(|v| v.as_i64()).unwrap_or(0).max(0);
    let (Some(char_id), Some(ship_id), Some(system)) = (ctx.session.char_id, ctx.session.ship_id, ctx.session.solar_system_id)
    else {
        return Outcome::custom("Modules can only be activated in space.");
    };
    let Some(effect) = effect_by_name(ctx, &name).await else {
        return Outcome::Unknown(format!("dogmaIM::Activate of unknown effect {name:?}"));
    };

    let running: HashSet<i64> = {
        let world = ctx.world.lock();
        let ship = world.parks.get(&system).and_then(|p| p.ships.get(&ship_id));
        match ship.and_then(|s| s.fight.as_ref()) {
            Some(f) if f.module(item_id).is_some() => return Outcome::Value(Value::None),
            Some(f) => f.modules.iter().map(|m| m.module_id).collect(),
            None => return Outcome::custom("Your ship is not in space."),
        }
    };
    let loadout = match Loadout::load(ctx.db, ctx.dogma, char_id, Some(ship_id), &running).await {
        Ok(l) => l,
        Err(e) => {
            error!("activating {item_id}: {}", crate::logging::chain(&e));
            return Outcome::custom("The module could not be read.");
        }
    };
    let planned = plan(&loadout, item_id, &effect, target, repeat);
    drop(loadout);
    let module = match planned {
        Ok(p) => finish(ctx, p).await,
        Err(why) => Err(why),
    };
    let module = match module {
        Ok(m) => m,
        Err(why) => {
            info!("activate {item_id} ({name}) refused: {why}");
            return Outcome::custom(why);
        }
    };

    // Into the fight, if the target is still good.
    let now = combat::now_ms();
    let passive = module.action == Action::Passive;
    let started = {
        let mut world = ctx.world.lock();
        let Some(park) = world.parks.get_mut(&system) else { return Outcome::custom("Your ship is not in space.") };
        let tick = park.tick;
        let check = check_target(park, ship_id, &module);
        let Some(fight) = park.ships.get_mut(&ship_id).and_then(|s| s.fight.as_mut()) else {
            return Outcome::custom("Your ship is not in space.");
        };
        if let Err(why) = check {
            return Outcome::custom(why);
        }
        if let Some(t) = module.target {
            if !fight.is_locked(t) {
                return user_error("TargetNotLocked", vec![("modulename", Value::text(module.name.clone())), ("targetname", Value::Int(t))]);
            }
        }
        let mut module = module;
        module.started_ms = now;
        module.next_ms = now;
        let event = fight::effect_event(char_id, ship_id, &module, true, now);
        let fx = fight::module_fx(tick, ship_id, &module, true);
        fight.modules.push(module.clone());
        world.notify(&[ctx.client_id], "OnGodmaShipEffect", event);
        space::broadcast(&world, system, None, vec![fx]);
        module
    };
    info!(
        "activated {} ({item_id}) {} on {:?}, {} ms cycle",
        started.name, effect.name, started.target, started.duration_ms
    );
    if passive {
        refit(ctx.db, ctx.dogma, ctx.world, ctx.client_id, char_id, ship_id, system).await;
    }
    Outcome::Value(Value::None)
}

/// Whether the module's target makes sense for what it does.
fn check_target(park: &crate::ballpark::Park, ship_id: i64, m: &Active) -> Result<(), String> {
    let Some(t) = m.target else { return Ok(()) };
    match m.action {
        Action::Turret { .. } | Action::Missile { .. } => {
            if !park.ships.get(&t).is_some_and(|s| s.fight.is_some()) {
                return Err("That is not something you can shoot.".into());
            }
        }
        Action::Mining { range, .. } => {
            if !park.rocks.contains_key(&t) {
                return Err("You can only mine asteroids.".into());
            }
            let distance = park.surface_distance(ship_id, t).unwrap_or(f64::MAX);
            if distance > range {
                return Err(format!("The asteroid is {distance:.0} m away; your {} reaches {range:.0} m.", m.name));
            }
        }
        ref a if a.projected() => {
            if !park.ships.get(&t).is_some_and(|s| s.fight.is_some()) {
                return Err("That only works on ships.".into());
            }
            let (optimal, falloff) = m.reach.unwrap_or((0.0, 0.0));
            let reach = optimal + 2.0 * falloff;
            let distance = park.surface_distance(ship_id, t).unwrap_or(f64::MAX);
            if distance > reach {
                return Err(format!("The target is {distance:.0} m away; your {} reaches {reach:.0} m.", m.name));
            }
        }
        _ => {}
    }
    Ok(())
}

/// A module's cycle as far as its attributes say; [`finish`] fills in what
/// needs the database.
struct Plan {
    module: Active,
    /// The charge it burns: (item, type, per cycle).
    burns: Option<(i64, i64, i64)>,
    /// A weapon with built-in ammunition of this type, whose damage is still
    /// to be read (times the damage multiplier).
    built_in: Option<(i64, f64)>,
}

/// The cycle a module runs, from its computed attributes and its charge's.
fn plan(loadout: &Loadout<'_>, item_id: i64, effect: &EffectRow, target: Option<i64>, repeat: i64) -> Result<Plan, String> {
    let Some(i) = loadout.find(item_id).filter(|i| loadout.pieces[*i].category_id == category::MODULE) else {
        return Err("That module is not fitted to your ship.".into());
    };
    let piece = &loadout.pieces[i];
    if piece.state == State::Offline {
        return Err("That module is offline.".into());
    }
    if !piece.has_effect(effect.id as i32) {
        return Err("That module cannot do that.".into());
    }
    let v = |a: i32| loadout.value(i, a);
    let duration = effect.duration_attr.map_or(0.0, v);
    if duration <= 0.0 {
        return Err("That module has no cycle to run.".into());
    }
    if effect.category == TARGETED && target.is_none() {
        return Err("That module needs a target.".into());
    }
    let charge = loadout.charge_of(i);
    let burns = |per_cycle: i64| {
        charge
            .map(|c| &loadout.pieces[c])
            .filter(|p| !REUSABLE_CHARGE_GROUPS.contains(&p.group_id))
            .map(|p| (p.item_id, p.type_id, per_cycle))
    };

    let mut burned = None;
    let mut built_in = None;
    let action = match effect.id {
        effect::TARGET_ATTACK | effect::PROJECTILE_FIRED => {
            let multiplier = v(attr::DAMAGE_MULTIPLIER);
            let base = match charge {
                Some(c) => attr::DAMAGE_TYPES.map(|d| loadout.value(c, d)),
                None if loadout.has(i, attr::AMMO_LOADED) => {
                    built_in = Some((v(attr::AMMO_LOADED) as i64, multiplier));
                    [0.0; 4]
                }
                None => return Err("Load some ammunition first.".into()),
            };
            let per_cycle = if loadout.has(i, attr::CHARGE_RATE) { v(attr::CHARGE_RATE).max(1.0) as i64 } else { 1 };
            burned = burns(per_cycle);
            Action::Turret {
                damage: base.map(|d| d * multiplier),
                gun: Gun {
                    optimal: v(attr::MAX_RANGE),
                    falloff: v(attr::FALLOFF),
                    tracking: v(attr::TRACKING_SPEED),
                    optimal_sig: v(attr::OPTIMAL_SIG_RADIUS),
                },
            }
        }
        effect::USE_MISSILES => {
            let Some(c) = charge else { return Err("Load some missiles first.".into()) };
            let m = |a: i32| loadout.value(c, a);
            burned = burns(1);
            let flight_ms = m(attr::EXPLOSION_DELAY);
            Action::Missile {
                damage: attr::DAMAGE_TYPES.map(m),
                flight_ms: flight_ms as i64,
                range: m(attr::MAX_VELOCITY) * flight_ms / 1_000.0,
                cloud: m(attr::AOE_CLOUD_SIZE),
                aoe_velocity: m(attr::AOE_VELOCITY),
            }
        }
        effect::SHIELD_BOOSTING => Action::ShieldBoost(v(attr::SHIELD_BONUS)),
        effect::ARMOR_REPAIR => Action::ArmorRepair(v(attr::ARMOR_DAMAGE_AMOUNT)),
        effect::STRUCTURE_REPAIR => Action::HullRepair(v(attr::STRUCTURE_DAMAGE_AMOUNT)),
        effect::POWER_BOOSTER => {
            let Some(c) = charge else { return Err("Load some capacitor booster charges first.".into()) };
            burned = burns(1);
            Action::CapBoost(loadout.value(c, attr::CAPACITOR_BONUS))
        }
        effect::MINING | effect::MINING_LASER => Action::Mining { amount: v(attr::MINING_AMOUNT), range: v(attr::MAX_RANGE) },
        effect::ENERGY_DESTABILIZATION | effect::ENERGY_DESTABILIZATION_NEW => {
            Action::Neutralize(v(attr::ENERGY_DESTABILIZATION_AMOUNT))
        }
        effect::ENERGY_VAMPIRE | effect::ENERGY_NOSFERATU => Action::Leech(v(attr::POWER_TRANSFER_AMOUNT)),
        effect::ENERGY_TRANSFER => Action::GiveCapacitor(v(attr::POWER_TRANSFER_AMOUNT)),
        effect::SHIELD_TRANSFER => Action::GiveShield(v(attr::SHIELD_BONUS)),
        effect::TARGET_ARMOR_REPAIR => Action::GiveArmor(v(attr::ARMOR_DAMAGE_AMOUNT)),
        effect::MODIFY_TARGET_SPEED | effect::DECREASE_TARGET_SPEED => {
            Action::Web((1.0 + v(attr::SPEED_FACTOR) / 100.0).clamp(0.01, 1.0))
        }
        effect::WARP_SCRAMBLE => Action::Scramble(v(attr::WARP_SCRAMBLE_STRENGTH).max(1.0)),
        effect::JAM => Action::Jam(attr::JAMMER_STRENGTHS.map(v)),
        effect::DAMPEN | effect::DAMPEN_TEST => Action::Dampen {
            range: multiplier_or_one(v(attr::MAX_TARGET_RANGE_MULTIPLIER)),
            resolution: multiplier_or_one(v(attr::SCAN_RESOLUTION_MULTIPLIER)),
        },
        effect::DISRUPT | effect::DISRUPT_TEST => Action::Disrupt {
            range: multiplier_or_one(v(attr::MAX_RANGE_MULTIPLIER)),
            tracking: multiplier_or_one(v(attr::TRACKING_SPEED_MULTIPLIER)),
        },
        effect::TARGET_PAINT | effect::TARGET_PAINT_2 => Action::Paint(1.0 + v(attr::SIGNATURE_RADIUS_BONUS) / 100.0),
        _ => Action::Passive,
    };
    let reach = action.projected().then(|| (effect.range_attr.map_or(0.0, v), effect.falloff_attr.map_or(0.0, v)));
    if action.projected() && effect.category != TARGETED {
        return Err("That module works on a target.".into());
    }
    // A missile's look lives on the charge's launching effect.
    let guid = match (effect.id, charge) {
        (effect::USE_MISSILES, Some(_)) => "effects.MissileDeployment".to_string(),
        _ => effect.guid.clone(),
    };
    let module = Active {
        module_id: item_id,
        type_id: piece.type_id,
        effect_id: effect.id,
        guid,
        offensive: effect.offensive,
        action,
        target: if effect.category == TARGETED { target } else { None },
        ammo: None,
        duration_ms: duration as i64,
        cap_need: effect.discharge_attr.map_or(0.0, v).max(0.0),
        started_ms: 0,
        next_ms: 0,
        cycles: 0,
        repeat: repeat.min(REPEAT_FOREVER),
        stopping: false,
        name: String::new(),
        reach,
        landed: true,
    };
    Ok(Plan { module, burns: burned, built_in })
}

/// The rest of a module's cycle: its name, its charges left, built-in damage.
async fn finish(ctx: &Ctx<'_>, plan: Plan) -> Result<Active, String> {
    let mut module = plan.module;
    module.name = type_name(ctx, module.type_id).await;
    if let Some((item_id, type_id, per_cycle)) = plan.burns {
        let quantity = ctx
            .query_one(r#"SELECT "quantity" FROM entity WHERE "itemID" = $1"#, &[&(item_id as i32)])
            .await
            .map_or(0, |r| r.get::<_, i32>(0) as i64);
        if quantity < per_cycle {
            return Err("That module is out of charges.".into());
        }
        module.ammo = Some(Ammo { item_id, type_id, quantity, per_cycle });
    }
    if let (Some((type_id, multiplier)), Action::Turret { damage, .. }) = (plan.built_in, &mut module.action) {
        *damage = type_damage(ctx, type_id).await.map(|d| d * multiplier);
    }
    Ok(module)
}

/// A multiplier attribute, where 0 (absent) means no change.
fn multiplier_or_one(v: f64) -> f64 {
    if v > 0.0 { v } else { 1.0 }
}

async fn type_name(ctx: &Ctx<'_>, type_id: i64) -> String {
    ctx.query_one(r#"SELECT COALESCE("typeName", '') FROM "invTypes" WHERE "typeID" = $1"#, &[&(type_id as i32)])
        .await
        .map_or_else(|| format!("type {type_id}"), |r| r.get(0))
}

/// A charge type's damage, for a weapon with built-in ammunition.
async fn type_damage(ctx: &Ctx<'_>, type_id: i64) -> combat::Damage {
    let rows = ctx
        .query(
            r#"SELECT "attributeID", coalesce("valueFloat", "valueInt"::float8) FROM "dgmTypeAttributes"
                WHERE "typeID" = $1 AND "attributeID" = ANY($2)"#,
            &[&(type_id as i32), &attr::DAMAGE_TYPES.to_vec()],
        )
        .await;
    let mut damage = [0.0; 4];
    for r in &rows {
        if let (Some(at), Some(value)) = (attr::DAMAGE_TYPES.iter().position(|a| *a == r.get::<_, i32>(0)), r.get::<_, Option<f64>>(1)) {
            damage[at] = value;
        }
    }
    damage
}

/// Deactivate(itemID, effectName): the module stops when its cycle ends.
async fn deactivate(ctx: &Ctx<'_>, call: &Call) -> Outcome {
    let item_id = call.arg_id(0);
    if effect_name(call.args.get(1)).as_deref() == Some("online") {
        return crate::svc::fitting::set_online(ctx, item_id, false).await;
    }
    let found = with_fight(ctx, |_, fight, _| match fight.module_mut(item_id) {
        Some(m) => {
            m.stopping = true;
            true
        }
        None => false,
    });
    if found != Some(true) {
        debug!("deactivate {item_id}: not running");
    }
    Outcome::Value(Value::None)
}

/// Recomputes a ship's attributes for the modules now running, moves the new
/// numbers into the park, and tells the pilot what changed.
pub async fn refit(
    db: &tokio_postgres::Client,
    catalog: &Catalog,
    world: &crate::world::World,
    client_id: i64,
    char_id: i64,
    ship_id: i64,
    system: i64,
) {
    let (before_set, now_set): (BTreeSet<i64>, BTreeSet<i64>) = {
        let w = world.lock();
        let Some(fight) = w.parks.get(&system).and_then(|p| p.ships.get(&ship_id)).and_then(|s| s.fight.as_ref()) else { return };
        let passive = fight.modules.iter().filter(|m| m.action == Action::Passive).map(|m| m.module_id).collect();
        (fight.applied.clone(), passive)
    };
    if before_set == now_set {
        return;
    }
    let as_hash = |s: &BTreeSet<i64>| s.iter().copied().collect::<HashSet<i64>>();
    let result = async {
        let before = Loadout::load(db, catalog, char_id, Some(ship_id), &as_hash(&before_set)).await?.snapshot();
        let after = Loadout::load(db, catalog, char_id, Some(ship_id), &as_hash(&now_set)).await?.snapshot();
        let p = profile(db, catalog, char_id, ship_id, &now_set, combat::now_ms()).await?;
        Ok::<_, tokio_postgres::Error>((before, after, p))
    }
    .await;
    let (before, after, p) = match result {
        Ok((b, a, Some(p))) => (b, a, p),
        Ok(_) => return,
        Err(e) => {
            error!("recomputing ship {ship_id}: {}", crate::logging::chain(&e));
            return;
        }
    };
    let mut w = world.lock();
    let Some(park) = w.parks.get_mut(&system) else { return };
    let tick = park.tick;
    let Some(ship) = park.ships.get_mut(&ship_id) else { return };
    let old_speed = ship.stats.max_velocity;
    let old_mass = ship.stats.mass;
    apply_profile(ship, p, combat::now_ms());
    let mut entries = Vec::new();
    if (ship.stats.max_velocity - old_speed).abs() > 1e-6 {
        entries.push(entry(tick, "SetMaxSpeed", vec![Value::Int(ship_id), Value::Double(ship.stats.max_velocity)]));
    }
    if (ship.stats.mass - old_mass).abs() > 1e-6 {
        entries.push(entry(tick, "SetBallMass", vec![Value::Int(ship_id), Value::Double(ship.stats.mass)]));
    }
    debug!("ship {ship_id} refitted: {:.0} m/s, {:.0} kg", ship.stats.max_velocity, ship.stats.mass);
    space::broadcast(&w, system, None, entries);
    let changes = crate::svc::fitting::attribute_changes(char_id, &before, &after);
    if !changes.is_empty() {
        w.notify(&[client_id], "OnModuleAttributeChanges", Value::Tuple(vec![Value::List(changes)]));
    }
}
