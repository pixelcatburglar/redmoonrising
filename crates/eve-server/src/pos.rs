//! Starbase structures in a solar system: control towers and what is
//! anchored around them, as the park holds them.
//!
//! A structure is an item in the system (its position in the entity row)
//! with a `posStructures` row for its state. The states and their timers are
//! the client's (playerOwned.py): launched, a structure floats free and
//! unanchored; anchoring fixes it in place, then after the type's
//! anchoringDelay it is anchored; onlining takes onliningDelay; unanchoring
//! takes unanchoringDelay, after which it floats free again and can be
//! scooped. The client times each step from the slim item's posTimestamp,
//! so the server only has to agree on when a step began.
//!
//! `RMR_STARBASE_SPEEDUP` shortens those timers for testing by back-dating
//! the timestamp, which the client's timers then agree with.
//!
//! Structures can be shot (fight.rs). A tower whose shield falls below a
//! quarter goes reinforced if it has strontium (svc/starbase.rs reinforce):
//! invulnerable until the posTimestamp the strontium bought, then online
//! again. A structure whose hull gives out is destroyed.

use std::sync::OnceLock;

use eve_marshal::Value;
use tokio_postgres::GenericClient;

use crate::ballpark::{Park, Vec3};
use crate::combat::{attr, Charged, Vitals};
use crate::destiny::{flags, mode, Ball};
use crate::heartbeat::{Job, Out};
use crate::svc::rows::{float, int, text};
use crate::svc::space::{self, entry};

/// playerOwned.py STRUCTURE_*.
pub mod state {
    pub const UNANCHORED: i16 = 0;
    pub const ANCHORED: i16 = 1;
    pub const ONLINING: i16 = 2;
    pub const REINFORCED: i16 = 3;
    pub const ONLINE: i16 = 4;
    pub const OPERATING: i16 = 5;
}

pub const CATEGORY_STRUCTURE: i32 = 23;
pub const GROUP_CONTROL_TOWER: i64 = 365;
const TYPE_FORCE_FIELD: i64 = 16103;
const ATTR_SHIELD_RADIUS: i32 = 680;
const ATTR_ANCHORING_DELAY: i32 = 556;
const ATTR_ONLINING_DELAY: i32 = 677;
const ATTR_UNANCHORING_DELAY: i32 = 676;
const TICKS_PER_MS: i64 = 10_000;
/// What an anchored structure weighs to destiny: immovable.
const IMMOVABLE: f64 = 1.0e34;

static SPEEDUP: OnceLock<f64> = OnceLock::new();

/// How many times faster than the client's timers starbase steps run.
pub fn set_speedup(speedup: f64) {
    let _ = SPEEDUP.set(speedup.max(1.0));
}

/// When a step of `delay` ticks that starts now should be stamped as having
/// started, so that it ends `delay / speedup` from now.
pub fn timer_start(now: i64, delay: i64) -> i64 {
    let speedup = SPEEDUP.get().copied().unwrap_or(1.0);
    now - delay + (delay as f64 / speedup) as i64
}

#[derive(Debug, Clone)]
pub struct Structure {
    pub id: i64,
    pub type_id: i64,
    pub group_id: i64,
    pub name: String,
    pub owner_id: i64,
    /// The owner's corporation (the owner itself, for a corporation).
    pub corp_id: i64,
    pub alliance_id: Option<i64>,
    pub pos: Vec3,
    pub radius: f64,
    pub state: i16,
    /// FILETIME the current timed step began; None once it is over.
    pub stamp: Option<i64>,
    /// For a structure other than a tower: the tower it is anchored to.
    pub tower: Option<i64>,
    /// anchoringDelay, onliningDelay, unanchoringDelay, in FILETIME ticks.
    pub delays: [i64; 3],
    pub vitals: Vitals,
    pub signature: f64,
    /// (character, ship) that last hurt it.
    pub last_attacker: Option<(i64, i64)>,
    /// Reinforcement was asked for since the shield last stood above a
    /// quarter (and found no strontium, if it is not reinforced).
    pub reinforce_asked: bool,
    /// For a tower: its force field's reach (shieldRadius), the harmonic its
    /// password makes, and whether its corporation and alliance pass.
    pub field_radius: f64,
    pub harmonic: i64,
    pub allow_corp: bool,
    pub allow_alliance: bool,
    /// The force field ball an online tower raises.
    pub field: Option<Field>,
    /// For a tower: whom its defences engage.
    pub sentry: crate::guard::Sentry,
    /// For a gun, missile or EW battery: what it is doing.
    pub guard: Option<crate::guard::Guard>,
}

/// A tower's force field: a ball of its own (type Force Field), centred on
/// the tower, solid to all but those it lets through.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub id: i64,
    pub radius: f64,
    pub harmonic: i64,
    pub corp_id: i64,
    pub alliance_id: i64,
}

impl Field {
    pub fn ball(&self, tower: &Structure) -> Ball {
        Ball {
            id: self.id,
            mode: mode::FIELD,
            radius: self.radius,
            pos: tower.pos.tuple(),
            flags: flags::CELESTIAL,
            mass: IMMOVABLE,
            corp_id: self.corp_id as i32,
            alliance_id: self.alliance_id as i32,
            harmonic: self.harmonic,
            max_velocity: 0.0,
            velocity: (0.0, 0.0, 0.0),
            agility: 1.0,
            speed_fraction: 0.0,
            name: "Force Field".into(),
        }
    }

    /// moonmining.GetForcefield finds it by group and owner.
    pub fn slim(&self, tower: &Structure) -> Value {
        space::slim(vec![
            ("itemID", Value::Int(self.id)),
            ("typeID", Value::Int(TYPE_FORCE_FIELD)),
            ("ownerID", Value::Int(tower.owner_id)),
            ("corpID", Value::Int(tower.corp_id)),
            ("allianceID", Value::int_or_none(tower.alliance_id)),
            ("controlTowerID", Value::Int(tower.id)),
        ])
    }
}

/// The harmonic a password makes: none for no password, else a hash that is
/// never the none (-1) or master (-2) value.
pub fn harmonic(password: &str) -> i64 {
    if password.is_empty() {
        return crate::destiny::NO_HARMONIC;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in password.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    (h & 0x3fff_ffff_ffff_ffff) as i64
}

impl Structure {
    pub fn is_tower(&self) -> bool {
        self.group_id == GROUP_CONTROL_TOWER
    }

    fn delay(&self) -> i64 {
        match self.state {
            state::ANCHORED => self.delays[0],
            state::ONLINING => self.delays[1],
            state::UNANCHORED => self.delays[2],
            _ => 0,
        }
    }

    /// Whether the current step's timer is still running.
    pub fn busy(&self, now: i64) -> bool {
        self.stamp.is_some_and(|s| now < s + self.delay())
    }

    pub fn fully_anchored(&self, now: i64) -> bool {
        self.state == state::ANCHORED && !self.busy(now)
    }

    pub fn fully_online(&self, now: i64) -> bool {
        match self.state {
            state::ONLINING => !self.busy(now),
            state::REINFORCED | state::ONLINE | state::OPERATING => true,
            _ => false,
        }
    }

    /// Floating free: unanchored, and done unanchoring.
    pub fn is_free(&self, now: i64) -> bool {
        self.state == state::UNANCHORED && !self.busy(now)
    }

    /// The field it should have up now: an online tower's, letting through
    /// its password, and its corporation and alliance if it says so.
    fn wanted_field(&self, now: i64) -> Option<(i64, i64, i64)> {
        if !self.is_tower() || !self.fully_online(now) || self.field_radius <= 0.0 {
            return None;
        }
        let corp = if self.allow_corp { self.corp_id } else { -1 };
        let alliance = if self.allow_alliance { self.alliance_id.unwrap_or(-1) } else { -1 };
        Some((self.harmonic, corp, alliance))
    }

    /// Fixed in space: anything but a structure done unanchoring. Ships
    /// bounce off it, on both sides.
    pub fn fixed(&self) -> bool {
        !(self.state == state::UNANCHORED && self.stamp.is_none())
    }

    /// Reinforced: no damage gets through until its timer runs out.
    pub fn invulnerable(&self) -> bool {
        self.state == state::REINFORCED
    }

    pub fn destroyed(&self) -> bool {
        self.vitals.hull_max > 0.0 && self.vitals.hull_damage >= self.vitals.hull_max - 1e-6
    }

    /// Online or on its way there: drawing on its tower.
    pub fn drawing(&self) -> bool {
        matches!(self.state, state::ONLINING | state::REINFORCED | state::ONLINE | state::OPERATING)
    }

    pub fn slim(&self) -> Value {
        space::slim(vec![
            ("itemID", Value::Int(self.id)),
            ("typeID", Value::Int(self.type_id)),
            ("ownerID", Value::Int(self.owner_id)),
            ("corpID", Value::Int(self.corp_id)),
            ("allianceID", Value::int_or_none(self.alliance_id)),
            ("posState", Value::Int(self.state.into())),
            ("posTimestamp", self.stamp.map_or(Value::None, Value::Long)),
            ("controlTowerID", Value::int_or_none(self.tower)),
            ("incapacitated", Value::Int(0)),
        ])
    }

    /// Free and passable while it floats; fixed and solid once anchored.
    pub fn ball(&self) -> Ball {
        let free = !self.fixed();
        Ball {
            id: self.id,
            mode: mode::STOP,
            radius: self.radius,
            pos: self.pos.tuple(),
            flags: if free { flags::IS_FREE | flags::IS_GLOBAL | flags::IS_INTERACTIVE } else { flags::CELESTIAL },
            mass: IMMOVABLE,
            corp_id: self.corp_id as i32,
            alliance_id: self.alliance_id.map_or(-1, |a| a as i32),
            harmonic: crate::destiny::NO_HARMONIC,
            max_velocity: 0.0,
            velocity: (0.0, 0.0, 0.0),
            agility: 1.0,
            speed_fraction: 0.0,
            name: self.name.clone(),
        }
    }
}

/// OnSlimItemChange(itemID, slim), as a destiny entry (michelle.py:810).
pub fn slim_change(tick: i64, s: &Structure) -> Value {
    entry(tick, "OnSlimItemChange", vec![Value::Int(s.id), s.slim()])
}

/// SetBallFree(ballID, isFree) and SetBallMassive(ballID, isMassive): a
/// structure coming loose, or being fixed in place.
pub fn free_entries(tick: i64, id: i64, free: bool) -> Vec<Value> {
    vec![
        entry(tick, "SetBallFree", vec![Value::Int(id), Value::Int(free.into())]),
        entry(tick, "SetBallMassive", vec![Value::Int(id), Value::Int((!free).into())]),
    ]
}

/// AddBalls for one structure.
pub fn add_entry(tick: i64, s: &Structure) -> Value {
    let blob = crate::destiny::encode(crate::destiny::stamp(tick), &[s.ball()]);
    let chunk = Value::Tuple(vec![Value::Bytes(blob), Value::List(vec![s.slim()]), Value::Dict(Vec::new())]);
    entry(tick, "AddBalls", vec![chunk])
}

/// The structures in a system, for a new park.
pub async fn load(db: &impl GenericClient, solar_system_id: i64) -> Result<Vec<Structure>, tokio_postgres::Error> {
    let rows = db
        .query(
            r#"SELECT e."itemID", e."typeID", t."groupID",
                      COALESCE(NULLIF(e."itemName", ''), t."typeName", ''), e."ownerID",
                      COALESCE(co."corporationID", ch."corporationID", e."ownerID"),
                      NULLIF(COALESCE(co."allianceID", cc."allianceID"), 0),
                      e."x", e."y", e."z", COALESCE(t."radius", 100),
                      COALESCE(p."state", 0), p."stateTimestamp", p."controlTowerID",
                      p."shieldCharge", COALESCE(p."armorDamage", 0), COALESCE(p."hullDamage", 0),
                      COALESCE(p."password", ''), COALESCE(p."allowCorp", 1), COALESCE(p."allowAlliance", 0),
                      p."sentryStanding", p."sentryStatus", COALESCE(p."sentryStandingDrop", 0),
                      COALESCE(p."sentryStatusDrop", 0), COALESCE(p."sentryAggression", 0), COALESCE(p."sentryWar", 1)
                 FROM entity e
                 JOIN "invTypes" t ON t."typeID" = e."typeID"
                 JOIN "invGroups" g ON g."groupID" = t."groupID"
                 LEFT JOIN "posStructures" p ON p."itemID" = e."itemID"
                 LEFT JOIN "corporation" co ON co."corporationID" = e."ownerID"
                 LEFT JOIN "character_" ch ON ch."characterID" = e."ownerID"
                 LEFT JOIN "corporation" cc ON cc."corporationID" = ch."corporationID"
                WHERE e."locationID" = $1 AND g."categoryID" = $2"#,
            &[&(solar_system_id as i32), &CATEGORY_STRUCTURE],
        )
        .await?;
    let types: Vec<i32> = rows.iter().filter_map(|r| int(r, 1)).map(|t| t as i32).collect();
    let delays = delays(db, &types).await?;
    let health = health(db, &types).await?;
    let now = crate::combat::now_ms();
    let mut structures: Vec<Structure> = rows
        .iter()
        .map(|r| {
            let type_id = int(r, 1).unwrap_or(0);
            Structure {
                id: int(r, 0).unwrap_or(0),
                type_id,
                group_id: int(r, 2).unwrap_or(0),
                name: text(r, 3).unwrap_or_default(),
                owner_id: int(r, 4).unwrap_or(0),
                corp_id: int(r, 5).unwrap_or(0),
                alliance_id: int(r, 6),
                pos: Vec3::new(float(r, 7).unwrap_or(0.0), float(r, 8).unwrap_or(0.0), float(r, 9).unwrap_or(0.0)),
                radius: float(r, 10).unwrap_or(100.0),
                state: int(r, 11).unwrap_or(0) as i16,
                stamp: int(r, 12),
                tower: int(r, 13),
                delays: delays.get(&type_id).copied().unwrap_or_default(),
                vitals: {
                    let mut v = vitals(health.get(&type_id), now);
                    if let Some(charge) = float(r, 14) {
                        v.shield.set(now, charge);
                    }
                    v.armor_damage = float(r, 15).unwrap_or(0.0);
                    v.hull_damage = float(r, 16).unwrap_or(0.0);
                    v
                },
                signature: health.get(&type_id).and_then(|a| a.get(&attr::SIGNATURE_RADIUS)).copied().unwrap_or(0.0),
                last_attacker: None,
                reinforce_asked: false,
                field_radius: health.get(&type_id).and_then(|a| a.get(&ATTR_SHIELD_RADIUS)).copied().unwrap_or(0.0),
                harmonic: harmonic(&text(r, 17).unwrap_or_default()),
                allow_corp: int(r, 18) == Some(1),
                allow_alliance: int(r, 19) == Some(1),
                field: None,
                sentry: crate::guard::Sentry {
                    standing: float(r, 20),
                    status: float(r, 21),
                    standing_drop: int(r, 22) == Some(1),
                    status_drop: int(r, 23) == Some(1),
                    aggression: int(r, 24) == Some(1),
                    war: int(r, 25) == Some(1),
                },
                guard: health.get(&type_id).and_then(|a| crate::guard::profile(int(r, 2).unwrap_or(0), a)),
            }
        })
        .collect();
    let batteries: Vec<i64> = structures.iter().filter(|s| s.guard.is_some()).map(|s| s.id).collect();
    if !batteries.is_empty() {
        let ammo = crate::guard::ammo(db, &batteries).await?;
        for s in &mut structures {
            if let Some(g) = s.guard.as_mut() {
                g.ammo = ammo.get(&s.id).cloned();
            }
        }
    }
    Ok(structures)
}

/// anchoringDelay, onliningDelay and unanchoringDelay of each type, in ticks.
pub async fn delays(db: &impl GenericClient, types: &[i32]) -> Result<std::collections::HashMap<i64, [i64; 3]>, tokio_postgres::Error> {
    let rows = db
        .query(
            r#"SELECT "typeID", "attributeID", COALESCE("valueFloat", "valueInt"::float8)
                 FROM "dgmTypeAttributes"
                WHERE "typeID" = ANY($1) AND "attributeID" = ANY($2)"#,
            &[&types, &vec![ATTR_ANCHORING_DELAY, ATTR_ONLINING_DELAY, ATTR_UNANCHORING_DELAY]],
        )
        .await?;
    let mut out: std::collections::HashMap<i64, [i64; 3]> = types.iter().map(|t| (i64::from(*t), [0; 3])).collect();
    for r in rows {
        let slot = match int(&r, 1).unwrap_or(0) as i32 {
            ATTR_ANCHORING_DELAY => 0,
            ATTR_ONLINING_DELAY => 1,
            _ => 2,
        };
        let ms = float(&r, 2).unwrap_or(0.0) as i64;
        out.entry(int(&r, 0).unwrap_or(0)).or_default()[slot] = ms * TICKS_PER_MS;
    }
    Ok(out)
}

/// The attributes a structure fights with, by type.
pub async fn health(
    db: &impl GenericClient,
    types: &[i32],
) -> Result<std::collections::HashMap<i64, std::collections::HashMap<i32, f64>>, tokio_postgres::Error> {
    let mut wanted = vec![attr::HP, attr::SHIELD_CAPACITY, attr::ARMOR_HP, attr::SHIELD_RECHARGE_RATE, attr::SIGNATURE_RADIUS, ATTR_SHIELD_RADIUS];
    wanted.extend(crate::guard::profile_attributes());
    wanted.extend(attr::SHIELD_RESONANCE);
    wanted.extend(attr::ARMOR_RESONANCE);
    wanted.extend(attr::HULL_RESONANCE);
    let rows = db
        .query(
            r#"SELECT "typeID", "attributeID", COALESCE("valueFloat", "valueInt"::float8)
                 FROM "dgmTypeAttributes" WHERE "typeID" = ANY($1) AND "attributeID" = ANY($2)"#,
            &[&types, &wanted],
        )
        .await?;
    let mut out: std::collections::HashMap<i64, std::collections::HashMap<i32, f64>> = std::collections::HashMap::new();
    for r in rows {
        if let (Some(t), Some(a), Some(v)) = (int(&r, 0), int(&r, 1), float(&r, 2)) {
            out.entry(t).or_default().insert(a as i32, v);
        }
    }
    Ok(out)
}

/// Full health from a type's attributes; resonances default to none.
pub fn vitals(a: Option<&std::collections::HashMap<i32, f64>>, now: i64) -> Vitals {
    let v = |id: i32, default: f64| a.and_then(|a| a.get(&id)).copied().unwrap_or(default);
    Vitals {
        shield: Charged::full(v(attr::SHIELD_CAPACITY, 0.0), v(attr::SHIELD_RECHARGE_RATE, 3_600_000.0), now),
        armor_max: v(attr::ARMOR_HP, 0.0),
        armor_damage: 0.0,
        hull_max: v(attr::HP, 1.0),
        hull_damage: 0.0,
        capacitor: Charged::full(0.0, 1.0, now),
        resonance: [
            attr::SHIELD_RESONANCE.map(|i| v(i, 1.0)),
            attr::ARMOR_RESONANCE.map(|i| v(i, 1.0)),
            attr::HULL_RESONANCE.map(|i| v(i, 1.0)),
        ],
    }
}

/// One pulse: steps whose timers ran out finish. Unanchoring sets the
/// structure loose; onlining brings it online.
pub fn step(park: &mut Park, out: &mut Out) {
    let now = crate::bulkdata::filetime_now();
    let now_ms = crate::combat::now_ms();
    let (tick, system) = (park.tick, park.solar_system_id);
    for s in park.structures.values_mut() {
        for e in reconcile_field(s, now, tick) {
            out.destiny(system, e);
        }
        // Recovered past a quarter shield: it may ask for reinforcement again.
        if s.reinforce_asked && !s.invulnerable() && s.vitals.damage_state(now_ms).0 >= 0.25 {
            s.reinforce_asked = false;
        }
        if s.stamp.is_none() || s.busy(now) {
            continue;
        }
        let loose = s.state == state::UNANCHORED;
        if matches!(s.state, state::ONLINING | state::REINFORCED) {
            s.state = state::ONLINE;
        }
        if loose {
            s.tower = None;
        }
        s.stamp = None;
        if loose {
            for e in free_entries(tick, s.id, true) {
                out.destiny(system, e);
            }
        }
        out.destiny(system, slim_change(tick, s));
        out.jobs.push(Job::Structure { id: s.id, state: s.state, stamp: None, tower: s.tower });
    }
}

/// Raises, lowers or retunes a tower's force field to match the tower;
/// returns the destiny entries that tell the system.
fn reconcile_field(s: &mut Structure, now: i64, tick: i64) -> Vec<Value> {
    let wanted = s.wanted_field(now);
    match (wanted, s.field.clone()) {
        (None, None) => Vec::new(),
        (None, Some(f)) => {
            s.field = None;
            vec![entry(tick, "RemoveBalls", vec![Value::List(vec![Value::Int(f.id)])])]
        }
        (Some((harmonic, corp_id, alliance_id)), None) => {
            let f = Field { id: crate::npc::next_id(), radius: s.field_radius, harmonic, corp_id, alliance_id };
            let blob = crate::destiny::encode(crate::destiny::stamp(tick), &[f.ball(s)]);
            let chunk = Value::Tuple(vec![Value::Bytes(blob), Value::List(vec![f.slim(s)]), Value::Dict(Vec::new())]);
            s.field = Some(f);
            vec![entry(tick, "AddBalls", vec![chunk])]
        }
        (Some(key), Some(f)) if key != (f.harmonic, f.corp_id, f.alliance_id) => {
            let (harmonic, corp_id, alliance_id) = key;
            s.field = Some(Field { harmonic, corp_id, alliance_id, ..f.clone() });
            vec![harmonic_entry(tick, f.id, key, true)]
        }
        _ => Vec::new(),
    }
}

/// SetBallHarmonic(ballID, harmonic, corporationID, allianceID, isField).
pub fn harmonic_entry(tick: i64, id: i64, (harmonic, corp, alliance): (i64, i64, i64), field: bool) -> Value {
    let args = vec![Value::Int(id), Value::Long(harmonic), Value::Int(corp), Value::Int(alliance), Value::Int(field.into())];
    entry(tick, "SetBallHarmonic", args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tower(state: i16, stamp: Option<i64>) -> Structure {
        Structure {
            id: 1,
            type_id: 12235,
            group_id: GROUP_CONTROL_TOWER,
            name: String::new(),
            owner_id: 2,
            corp_id: 2,
            alliance_id: None,
            pos: Vec3::ZERO,
            radius: 3000.0,
            state,
            stamp,
            tower: None,
            delays: [100, 200, 300],
            vitals: vitals(None, 0),
            signature: 0.0,
            last_attacker: None,
            reinforce_asked: false,
            field_radius: 30_000.0,
            harmonic: -1,
            allow_corp: true,
            allow_alliance: false,
            field: None,
            sentry: Default::default(),
            guard: None,
        }
    }

    #[test]
    fn steps_finish_when_their_delay_has_passed() {
        assert!(!tower(state::ANCHORED, Some(1000)).fully_anchored(1099));
        assert!(tower(state::ANCHORED, Some(1000)).fully_anchored(1100));
        assert!(!tower(state::ONLINING, Some(1000)).fully_online(1199));
        assert!(tower(state::ONLINING, Some(1000)).fully_online(1200));
        assert!(!tower(state::UNANCHORED, Some(1000)).is_free(1299));
        assert!(tower(state::UNANCHORED, None).is_free(0));
    }

    #[test]
    fn an_online_tower_raises_its_field_and_lowers_it_offline() {
        let mut t = tower(state::ONLINE, None);
        let raised = reconcile_field(&mut t, 0, 1);
        assert_eq!(raised.len(), 1);
        assert!(t.field.as_ref().is_some_and(|f| f.corp_id == 2 && f.alliance_id == -1));
        assert!(reconcile_field(&mut t, 0, 2).is_empty(), "nothing changed");
        t.harmonic = harmonic("secret");
        assert_eq!(reconcile_field(&mut t, 0, 3).len(), 1, "retuned");
        t.state = state::ANCHORED;
        assert_eq!(reconcile_field(&mut t, 0, 4).len(), 1);
        assert!(t.field.is_none());
    }

    #[test]
    fn passwords_make_harmonics_that_are_never_the_special_values() {
        assert_eq!(harmonic(""), -1);
        assert!(harmonic("x") >= 0);
        assert_eq!(harmonic("x"), harmonic("x"));
    }
}
