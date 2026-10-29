//! What lives in a solar system's asteroid belts: the asteroids, and the
//! pirates that wait among them for whoever comes to mine.
//!
//! A system's [`Wildlife`] is read from the database when its park is made:
//! which pirate faction holds it (from the system's or region's faction), the
//! rats of that faction by hull class, and the ores its security allows. Each
//! belt gets a field of asteroids at once; its rats spawn when a player comes
//! within [`SPAWN_RANGE`] and respawn some minutes after the last one dies.
//!
//! Rats are ordinary ships in the park with a [`Brain`]: they pick the nearest
//! player in range (or whoever shoots them), lock, orbit at their preferred
//! range and fire on the module cycles `fight` runs for everyone.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};

use eve_marshal::Value;
use tracing::{debug, info};

use crate::ballpark::{Command, Park, Pilot, Ship, ShipStats, Vec3};
use crate::combat::{self, attr, Action, Active, Charged, Damage, Fighter, Gun, Lock, Vitals, REPEAT_FOREVER};
use crate::heartbeat::Out;
use crate::svc::space;

/// Players this close to a belt wake its rats.
pub const SPAWN_RANGE: f64 = 150_000.0;
/// How long a cleared belt stays empty, ms.
const RESPAWN_MS: (f64, f64) = (240_000.0, 480_000.0);
/// Rats notice players this far off, at least.
const MIN_AGGRO_RANGE: f64 = 30_000.0;
/// A rat gives up on a target this far from its belt.
const LEASH: f64 = 120_000.0;
const THINK_MS: i64 = 1_000;
const GROUP_BELT: i64 = 9;
const GROUP_CONCORD: i32 = 301;
const OWNER_SYSTEM: i64 = 1;

/// Transient objects (rats, asteroids) get IDs from here: above every item
/// the database will make for a long time, below destiny's 32-bit limit.
static NEXT_ID: AtomicI64 = AtomicI64::new(1_500_000_000);

pub fn next_id() -> i64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Hull classes, smallest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Frigate,
    Destroyer,
    Cruiser,
    Battlecruiser,
    Battleship,
}

const CLASSES: [Class; 5] = [Class::Frigate, Class::Destroyer, Class::Cruiser, Class::Battlecruiser, Class::Battleship];

/// A pirate faction's belt rat groups (in [`CLASSES`] order) and the
/// corporation its rats belong to.
struct Pirates {
    name: &'static str,
    corp_id: i64,
    groups: [i32; 5],
}

const ANGELS: Pirates = Pirates { name: "Angel Cartel", corp_id: 1000124, groups: [550, 575, 551, 576, 552] };
const BLOOD_RAIDERS: Pirates = Pirates { name: "Blood Raiders", corp_id: 1000134, groups: [557, 577, 555, 578, 556] };
const GURISTAS: Pirates = Pirates { name: "Guristas", corp_id: 1000127, groups: [562, 579, 561, 580, 560] };
const SANSHA: Pirates = Pirates { name: "Sansha's Nation", corp_id: 1000162, groups: [567, 581, 566, 582, 565] };
const SERPENTIS: Pirates = Pirates { name: "Serpentis", corp_id: 1000135, groups: [572, 583, 571, 584, 570] };
const ALL_PIRATES: [&Pirates; 5] = [&ANGELS, &BLOOD_RAIDERS, &GURISTAS, &SANSHA, &SERPENTIS];

/// Who raids an empire's space; a pirate faction's own space is its own.
fn pirates_for(faction_id: Option<i64>, solar_system_id: i64) -> &'static Pirates {
    match faction_id {
        Some(500001 | 500010) => &GURISTAS,
        Some(500002 | 500011 | 500015) => &ANGELS,
        Some(500003 | 500007 | 500008 | 500012) => &BLOOD_RAIDERS,
        Some(500019) => &SANSHA,
        Some(500004 | 500009 | 500014 | 500020) => &SERPENTIS,
        _ => ALL_PIRATES[(solar_system_id % ALL_PIRATES.len() as i64) as usize],
    }
}

/// Ores by the lowest security they appear in: (typeID, security floor).
const ORES: [(i64, f64); 16] = [
    (1230, -1.0), // Veldspar
    (1228, -1.0), // Scordite
    (1224, 0.7),  // Pyroxeres
    (18, 0.5),    // Plagioclase
    (1227, 0.4),  // Omber
    (20, 0.4),    // Kernite
    (1226, 0.2),  // Jaspet
    (1231, 0.2),  // Hemorphite
    (21, 0.2),    // Hedbergite
    (1229, 0.0),  // Gneiss
    (1232, 0.0),  // Dark Ochre
    (19, 0.0),    // Spodumain
    (1225, 0.0),  // Crokite
    (1223, 0.0),  // Bistot
    (22, 0.0),    // Arkonor
    (11396, 0.0), // Mercoxit
];

#[derive(Debug, Clone)]
pub struct NpcType {
    pub type_id: i64,
    pub group_id: i64,
    pub name: String,
    pub radius: f64,
    pub mass: f64,
    attrs: HashMap<i32, f64>,
}

impl NpcType {
    pub fn get(&self, a: i32) -> f64 {
        self.attrs.get(&a).copied().unwrap_or(0.0)
    }

    /// This type with some attributes as computed for one item (a drone with
    /// its pilot's skills applied).
    pub fn with_attributes(&self, computed: Option<&HashMap<i32, f64>>) -> NpcType {
        let mut t = self.clone();
        if let Some(c) = computed {
            t.attrs.extend(c.iter().map(|(a, v)| (*a, *v)));
        }
        t
    }

    fn bounty(&self) -> f64 {
        self.get(attr::ENTITY_KILL_BOUNTY)
    }

    /// Whether it carries a gun or a missile it can use.
    fn armed(&self, missiles: &HashMap<i64, NpcType>) -> bool {
        let gun = self.get(attr::DAMAGE_MULTIPLIER) > 0.0 && self.get(attr::SPEED) > 0.0;
        let missile = missiles.contains_key(&(self.get(attr::ENTITY_MISSILE_TYPE_ID) as i64));
        gun || missile
    }
}

#[derive(Debug, Clone)]
pub struct Ore {
    pub type_id: i64,
    pub name: String,
    pub volume: f64,
}

/// What a solar system's belts hold.
#[derive(Debug, Clone, Default)]
pub struct Wildlife {
    pub security: f64,
    pub corp_id: i64,
    /// Rat types by class, weakest (by bounty) first.
    rats: [Vec<NpcType>; 5],
    /// The missiles rats fire, by type.
    missiles: HashMap<i64, NpcType>,
    pub ores: Vec<Ore>,
    /// CONCORD's ships, for when someone commits a crime here.
    pub police: Vec<NpcType>,
}

/// An asteroid.
#[derive(Debug, Clone)]
pub struct Rock {
    pub id: i64,
    pub type_id: i64,
    pub name: String,
    pub pos: Vec3,
    pub radius: f64,
    /// Units of ore left.
    pub quantity: i64,
    /// m3 per unit.
    pub volume: f64,
}

#[derive(Debug, Clone)]
pub struct Belt {
    pub id: i64,
    pub pos: Vec3,
    pub rats: Vec<i64>,
    /// When the rats may come (back); None until first visited.
    pub respawn_at: Option<i64>,
}

/// A missile in flight.
#[derive(Debug, Clone)]
pub struct Missile {
    pub attacker: combat_ref::Attacker,
    pub target: i64,
    pub impact_ms: i64,
    pub damage: Damage,
    pub cloud: f64,
    pub aoe_velocity: f64,
    /// False if the target is beyond the missile's flight range.
    pub reaches: bool,
    pub weapon: String,
}

/// Who fired, as it was at the time (it may be gone when the shot lands).
pub mod combat_ref {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Attacker {
        pub ship_id: i64,
        pub char_id: i64,
        /// The connection to tell, if a player fired.
        pub client_id: Option<i64>,
        /// For the kill report: what it flew and fired.
        pub ship_type: i64,
        pub weapon_type: i64,
    }
}

/// A periodic self-repair.
#[derive(Debug, Clone, Copy)]
struct Heal {
    amount: f64,
    every_ms: i64,
    next_ms: i64,
}

#[derive(Debug, Clone)]
pub struct Brain {
    pub belt: i64,
    pub bounty: f64,
    pub target: Option<i64>,
    attack_range: f64,
    fly_range: f64,
    /// Orbit speed as a fraction of top speed.
    cruise: f64,
    next_think_ms: i64,
    /// The weapons it fires once locked, ready to be aimed.
    weapons: Vec<Active>,
    shield_boost: Option<Heal>,
    armor_repair: Option<Heal>,
    chasing: bool,
    /// What killing it does for a pilot's security status: a share of the
    /// way to 10, but not past the maximum.
    pub security_bonus: f64,
    pub security_max_gain: f64,
    /// How many stacks of loot it drops, at least and at most.
    pub loot_count: (usize, usize),
    /// CONCORD: here for one criminal, gone when done.
    pub police: Option<Police>,
}

#[derive(Debug, Clone, Copy)]
pub struct Police {
    pub criminal: i64,
    /// When it leaves if the criminal has gone.
    pub leave_ms: i64,
}

impl Brain {
    /// Someone shot it: shoot back, unless it is busy with someone else.
    pub fn provoked(&mut self, by: i64) {
        if self.target.is_none() {
            self.target = Some(by);
            self.next_think_ms = 0;
        }
    }

    fn aggro_range(&self) -> f64 {
        (self.attack_range * 2.0).max(MIN_AGGRO_RANGE)
    }
}

// ---- loading ------------------------------------------------------------------------

pub async fn load(db: &tokio_postgres::Client, solar_system_id: i64) -> Result<Wildlife, tokio_postgres::Error> {
    let row = db
        .query_opt(
            r#"SELECT COALESCE(s."security", 0)::float8, COALESCE(s."factionID", r."factionID")
                 FROM "mapSolarSystems" s
                 LEFT JOIN "mapRegions" r ON r."regionID" = s."regionID"
                WHERE s."solarSystemID" = $1"#,
            &[&(solar_system_id as i32)],
        )
        .await?;
    let (security, faction) = row.map_or((1.0, None), |r| (r.get(0), r.get::<_, Option<i32>>(1).map(i64::from)));
    let pirates = pirates_for(faction, solar_system_id);

    let types = db
        .query(
            r#"SELECT t."typeID", t."groupID", COALESCE(t."typeName", ''), COALESCE(t."radius", 0), COALESCE(t."mass", 0)
                 FROM "invTypes" t WHERE t."groupID" = ANY($1)"#,
            &[&&pirates.groups[..]],
        )
        .await?;
    let mut by_id: HashMap<i64, NpcType> = types.iter().map(|r| npc_type(r)).map(|t| (t.type_id, t)).collect();
    fill_attributes(db, &mut by_id).await?;

    let missile_ids: Vec<i32> = by_id
        .values()
        .map(|t| t.get(attr::ENTITY_MISSILE_TYPE_ID) as i32)
        .filter(|id| *id > 0)
        .collect();
    let rows = db
        .query(
            r#"SELECT t."typeID", t."groupID", COALESCE(t."typeName", ''), COALESCE(t."radius", 0), COALESCE(t."mass", 0)
                 FROM "invTypes" t WHERE t."typeID" = ANY($1)"#,
            &[&missile_ids],
        )
        .await?;
    let mut missiles: HashMap<i64, NpcType> = rows.iter().map(|r| npc_type(r)).map(|t| (t.type_id, t)).collect();
    fill_attributes(db, &mut missiles).await?;

    let mut rats: [Vec<NpcType>; 5] = Default::default();
    for t in by_id.into_values() {
        if t.bounty() <= 0.0 || t.get(attr::HP) <= 0.0 || !t.armed(&missiles) {
            continue;
        }
        if let Some(class) = pirates.groups.iter().position(|g| *g as i64 == t.group_id) {
            rats[class].push(t);
        }
    }
    for list in rats.iter_mut() {
        list.sort_by(|a, b| a.bounty().total_cmp(&b.bounty()).then(a.type_id.cmp(&b.type_id)));
    }

    let ore_ids: Vec<i32> = ORES.iter().filter(|(_, floor)| security >= *floor).map(|(id, _)| *id as i32).collect();
    let ores = db
        .query(r#"SELECT "typeID", COALESCE("typeName", ''), COALESCE("volume", 1) FROM "invTypes" WHERE "typeID" = ANY($1)"#, &[&ore_ids])
        .await?
        .iter()
        .map(|r| Ore {
            type_id: r.get::<_, i32>(0) as i64,
            name: r.get(1),
            volume: r.get::<_, f64>(2).max(0.01),
        })
        .collect::<Vec<_>>();

    let counts: Vec<usize> = rats.iter().map(Vec::len).collect();
    debug!(
        "system {solar_system_id}: security {security:.2}, {} rats {counts:?}, {} ores",
        pirates.name,
        ores.len()
    );
    let police: Vec<NpcType> = load_group(db, GROUP_CONCORD).await?;
    Ok(Wildlife { security, corp_id: pirates.corp_id, rats, missiles, ores, police })
}

/// Every type in a group, with its attributes.
async fn load_group(db: &tokio_postgres::Client, group_id: i32) -> Result<Vec<NpcType>, tokio_postgres::Error> {
    let ids: Vec<i64> = db
        .query(r#"SELECT "typeID" FROM "invTypes" WHERE "groupID" = $1"#, &[&group_id])
        .await?
        .iter()
        .map(|r| r.get::<_, i32>(0) as i64)
        .collect();
    let mut types: Vec<NpcType> = load_types(db, &ids).await?.into_values().collect();
    types.sort_by_key(|t| t.type_id);
    Ok(types)
}

/// Types with their attributes, for entities that are not belt rats (drones,
/// CONCORD).
pub async fn load_types(db: &tokio_postgres::Client, type_ids: &[i64]) -> Result<HashMap<i64, NpcType>, tokio_postgres::Error> {
    let ids: Vec<i32> = type_ids.iter().map(|t| *t as i32).collect();
    let rows = db
        .query(
            r#"SELECT t."typeID", t."groupID", COALESCE(t."typeName", ''), COALESCE(t."radius", 0), COALESCE(t."mass", 0)
                 FROM "invTypes" t WHERE t."typeID" = ANY($1)"#,
            &[&ids],
        )
        .await?;
    let mut types: HashMap<i64, NpcType> = rows.iter().map(npc_type).map(|t| (t.type_id, t)).collect();
    fill_attributes(db, &mut types).await?;
    Ok(types)
}

fn npc_type(r: &tokio_postgres::Row) -> NpcType {
    NpcType {
        type_id: r.get::<_, i32>(0) as i64,
        group_id: r.get::<_, i32>(1) as i64,
        name: r.get(2),
        radius: r.get::<_, f64>(3),
        mass: r.get::<_, f64>(4),
        attrs: HashMap::new(),
    }
}

async fn fill_attributes(db: &tokio_postgres::Client, types: &mut HashMap<i64, NpcType>) -> Result<(), tokio_postgres::Error> {
    let ids: Vec<i32> = types.keys().map(|id| *id as i32).collect();
    let rows = db
        .query(
            r#"SELECT "typeID", "attributeID", coalesce("valueFloat", "valueInt"::float8)
                 FROM "dgmTypeAttributes" WHERE "typeID" = ANY($1)"#,
            &[&ids],
        )
        .await?;
    for r in &rows {
        if let (Some(t), Some(v)) = (types.get_mut(&(r.get::<_, i32>(0) as i64)), r.get::<_, Option<f64>>(2)) {
            t.attrs.insert(r.get(1), v);
        }
    }
    Ok(())
}

// ---- the belts ------------------------------------------------------------------------

/// Lays out the belts of a new park and fills them with asteroids.
pub fn populate(park: &mut Park) {
    let belts: Vec<(i64, Vec3)> =
        park.celestials.iter().filter(|c| c.group_id == GROUP_BELT).map(|c| (c.id, c.pos)).collect();
    let ores = park.wildlife.ores.clone();
    for (id, pos) in belts {
        park.belts.push(Belt { id, pos, rats: Vec::new(), respawn_at: None });
        if ores.is_empty() {
            continue;
        }
        let count = 8 + park.rng.below(8);
        for _ in 0..count {
            // The common ores are the common ones.
            let pick = park.rng.below(ores.len()).min(park.rng.below(ores.len()));
            let ore = &ores[pick];
            // Roughly 2,000 to 10,000 m3 of ore.
            let quantity = (park.rng.range(2_000.0, 10_000.0) / ore.volume).round().max(1.0) as i64;
            let offset = park.rng.in_sphere(25_000.0);
            let rock = Rock {
                id: next_id(),
                type_id: ore.type_id,
                name: ore.name.clone(),
                pos: pos + Vec3::new(offset.x, offset.y * 0.2, offset.z),
                radius: rock_radius(quantity as f64 * ore.volume),
                quantity,
                volume: ore.volume,
            };
            park.rocks.insert(rock.id, rock);
        }
    }
    info!(
        "system {}: {} belt(s), {} asteroid(s)",
        park.solar_system_id,
        park.belts.len(),
        park.rocks.len()
    );
}

/// An asteroid's size from the ore in it.
fn rock_radius(m3: f64) -> f64 {
    (800.0 + 40.0 * m3.cbrt()).min(12_000.0)
}

/// Asteroids are celestial-like; this is their slim.
pub fn rock_slim(rock: &Rock) -> Value {
    space::slim(vec![
        ("itemID", Value::Int(rock.id)),
        ("typeID", Value::Int(rock.type_id)),
        ("ownerID", Value::Int(OWNER_SYSTEM)),
    ])
}

// ---- thinking -------------------------------------------------------------------------

/// Spawns rats where players have come, and lets every rat think.
pub fn think(park: &mut Park, now: i64, out: &mut Out) {
    let players: Vec<(i64, Vec3)> =
        park.ships.values().filter(|s| s.is_player() && !s.warping()).map(|s| (s.id, s.pos)).collect();
    spawn_where_needed(park, now, &players, out);
    let due: Vec<i64> = park
        .ships
        .values()
        .filter(|s| s.npc.as_ref().is_some_and(|b| b.police.is_none() && b.next_think_ms <= now))
        .map(|s| s.id)
        .collect();
    for id in due {
        think_one(park, id, now, &players, out);
    }
}

fn spawn_where_needed(park: &mut Park, now: i64, players: &[(i64, Vec3)], out: &mut Out) {
    for i in 0..park.belts.len() {
        let belt = &park.belts[i];
        if !belt.rats.is_empty() || belt.respawn_at.is_some_and(|t| t > now) {
            continue;
        }
        if players.iter().any(|(_, p)| (*p - belt.pos).len() < SPAWN_RANGE) {
            spawn(park, i, now, out);
        }
    }
}

/// How many of which class, by security: frigates in high-sec, bigger hulls
/// the lower it goes.
fn plan(security: f64, rng: &mut combat::Rng) -> Vec<Class> {
    let n = |rng: &mut combat::Rng, lo: usize, hi: usize| lo + rng.below(hi - lo + 1);
    let mut out = Vec::new();
    let mut add = |class: Class, count: usize| out.extend(std::iter::repeat(class).take(count));
    match security {
        s if s >= 0.7 => add(Class::Frigate, n(rng, 1, 3)),
        s if s >= 0.5 => {
            add(Class::Frigate, n(rng, 2, 3));
            add(Class::Destroyer, n(rng, 0, 1));
        }
        s if s > 0.0 => {
            add(Class::Cruiser, n(rng, 1, 2));
            add(Class::Destroyer, n(rng, 0, 2));
            add(Class::Frigate, n(rng, 1, 2));
        }
        s if s > -0.5 => {
            add(Class::Battlecruiser, n(rng, 0, 1));
            add(Class::Cruiser, n(rng, 2, 3));
            add(Class::Frigate, n(rng, 0, 2));
        }
        _ => {
            add(Class::Battleship, n(rng, 1, 2));
            add(Class::Cruiser, n(rng, 1, 3));
        }
    }
    out
}

/// Which third of a class's rats (by bounty) a system gets.
fn tier(security: f64, len: usize) -> std::ops::Range<usize> {
    let third = (len / 3).max(1);
    let start = match security {
        s if s >= 0.5 => 0,
        s if s > 0.0 => third,
        _ => len.saturating_sub(third),
    };
    start.min(len.saturating_sub(1))..(start + third).min(len)
}

fn spawn(park: &mut Park, belt_index: usize, now: i64, out: &mut Out) {
    let wildlife = std::sync::Arc::clone(&park.wildlife);
    let belt_id = park.belts[belt_index].id;
    let centre = park.belts[belt_index].pos;
    let mut ids = Vec::new();
    for class in plan(wildlife.security, &mut park.rng) {
        // A class this faction has none of falls back to the next smaller one.
        let index = CLASSES.iter().position(|c| *c == class).unwrap_or(0);
        let Some(list) = (0..=index).rev().map(|i| &wildlife.rats[i]).find(|l| !l.is_empty()) else { continue };
        let range = tier(wildlife.security, list.len());
        let t = &list[range.start + park.rng.below(range.len())];
        let offset = park.rng.in_sphere(20_000.0);
        let rat = make_rat(t, &wildlife, belt_id, centre + Vec3::new(offset.x, offset.y * 0.2, offset.z), now, &mut park.rng);
        ids.push(rat.id);
        park.ships.insert(rat.id, rat);
    }
    park.belts[belt_index].rats = ids.clone();
    if ids.is_empty() {
        // Nothing to spawn here; do not try again every pulse.
        park.belts[belt_index].respawn_at = Some(now + RESPAWN_MS.1 as i64);
        return;
    }
    info!("belt {belt_id}: {} rat(s) spawned", ids.len());
    for id in ids {
        for e in space::add_ball_entries(park, id) {
            out.destiny(park.solar_system_id, e);
        }
    }
}

fn make_rat(t: &NpcType, wildlife: &Wildlife, belt: i64, pos: Vec3, now: i64, rng: &mut combat::Rng) -> Ship {
    let id = next_id();
    let a = |x: i32| t.get(x);
    let cruise_speed = a(attr::ENTITY_CRUISE_SPEED).max(50.0);
    // Their listed top speed is a chase speed; keep it within reason.
    let top = a(attr::MAX_VELOCITY).clamp(cruise_speed, cruise_speed * 2.5);
    let agility = if a(attr::AGILITY) > 0.0 { a(attr::AGILITY) } else { 1.0 };
    let mass = if t.mass > 0.0 { t.mass } else { 1_000_000.0 };
    let stats = ShipStats::new(t.radius.max(20.0), mass, agility, top, 1.0);
    let pilot = Pilot {
        type_id: t.type_id,
        group_id: t.group_id,
        name: t.name.clone(),
        char_id: 0,
        corp_id: wildlife.corp_id,
        alliance_id: None,
        security: 0.0,
        bounty: 0.0,
    };
    let attack_range = a(attr::ENTITY_ATTACK_RANGE).max(a(attr::MAX_RANGE)).max(5_000.0);
    let fly_range = a(attr::ENTITY_FLY_RANGE).max(1_000.0);
    let fighter = entity_fighter(t, attack_range * 2.0, 1, top, now);
    let heal = |amount: i32, every: i32| {
        let (amount, every) = (a(amount), a(every) as i64);
        (amount > 0.0 && every > 0).then_some(Heal { amount, every_ms: every, next_ms: now })
    };
    let brain = Brain {
        belt,
        bounty: t.bounty(),
        target: None,
        attack_range,
        fly_range,
        cruise: (cruise_speed / top).clamp(0.1, 1.0),
        next_think_ms: now + rng.range(0.0, THINK_MS as f64) as i64,
        weapons: weapons(t, &wildlife.missiles, id, rng),
        shield_boost: heal(attr::ENTITY_SHIELD_BOOST_AMOUNT, attr::ENTITY_SHIELD_BOOST_DURATION),
        armor_repair: heal(attr::ENTITY_ARMOR_REPAIR_AMOUNT, attr::ENTITY_ARMOR_REPAIR_DURATION),
        chasing: false,
        security_bonus: a(attr::ENTITY_SECURITY_STATUS_KILL_BONUS),
        security_max_gain: a(attr::ENTITY_SECURITY_MAX_GAIN),
        loot_count: (a(attr::ENTITY_LOOT_COUNT_MIN) as usize, a(attr::ENTITY_LOOT_COUNT_MAX) as usize),
        police: None,
    };
    let mut ship = Ship::new(id, 0, stats, pilot, pos, Vec3::ZERO);
    ship.fight = Some(fighter);
    ship.npc = Some(brain);
    ship.command = Command::Orbit { target: belt, range: fly_range.max(5_000.0) };
    ship.speed_fraction = (cruise_speed / top).clamp(0.1, 1.0);
    ship
}

/// A CONCORD ship come for `criminal`: nothing it carries is fair, and
/// nothing a criminal has will stop it.
pub fn police_ship(t: &NpcType, criminal: i64, pos: Vec3, now: i64) -> Ship {
    const CONCORD: i64 = 1000125;
    let id = next_id();
    let top = t.get(attr::MAX_VELOCITY).max(1_000.0);
    let stats = ShipStats::new(t.radius.max(50.0), t.mass.max(1_000_000.0), 0.5, top, 5.0);
    let pilot = Pilot {
        type_id: t.type_id,
        group_id: t.group_id,
        name: t.name.clone(),
        char_id: 0,
        corp_id: CONCORD,
        alliance_id: None,
        security: 0.0,
        bounty: 0.0,
    };
    let mut fighter = entity_fighter(t, 250_000.0, 1, top, now);
    fighter.vitals.resonance = [[0.0; 4]; 3];
    fighter.vitals.hull_max = fighter.vitals.hull_max.max(1.0e6);
    fighter.scan_resolution = 5_000.0;
    fighter.sensors = [1_000.0; 4];
    let gun = Gun { optimal: 250_000.0, falloff: 50_000.0, tracking: 100.0, optimal_sig: 1.0 };
    let mut weapons = vec![entity_module(t, slot::gun(id), EFFECT_TARGET_ATTACK, GUID_TURRET, Action::Turret { damage: [0.0, 0.0, 2_500.0, 2_500.0], gun }, 2_000.0)];
    let mut scram = entity_module(t, slot::scrambler(id), EFFECT_WARP_SCRAMBLE_FOR_ENTITY, GUID_SCRAMBLE, Action::Scramble(100.0), 5_000.0);
    scram.reach = Some((250_000.0, 0.0));
    weapons.push(scram);
    let brain = Brain {
        belt: 0,
        bounty: 0.0,
        target: None,
        attack_range: 250_000.0,
        fly_range: 2_500.0,
        cruise: 1.0,
        next_think_ms: now,
        weapons,
        shield_boost: None,
        armor_repair: None,
        chasing: false,
        security_bonus: 0.0,
        security_max_gain: 0.0,
        loot_count: (0, 0),
        police: Some(Police { criminal, leave_ms: now + POLICE_LINGER_MS }),
    };
    let mut ship = Ship::new(id, 0, stats, pilot, pos, Vec3::ZERO);
    ship.fight = Some(fighter);
    ship.npc = Some(brain);
    ship
}

/// How long CONCORD stays once there is no one left to punish.
pub const POLICE_LINGER_MS: i64 = 30_000;

impl Brain {
    /// The weapons it fires once locked, ready to be aimed.
    pub fn arms(&self) -> &[Active] {
        &self.weapons
    }
}

fn resonance(t: &NpcType, a: i32) -> f64 {
    t.attrs.get(&a).copied().unwrap_or(1.0)
}

/// Hit points, resistances and sensors of an entity type (a rat, a drone, a
/// policeman), at full health.
pub fn entity_fighter(t: &NpcType, lock_range: f64, max_locked: usize, top_speed: f64, now: i64) -> Fighter {
    let a = |x: i32| t.get(x);
    let vitals = Vitals {
        shield: Charged::full(a(attr::SHIELD_CAPACITY), a(attr::SHIELD_RECHARGE_RATE), now),
        armor_max: a(attr::ARMOR_HP),
        armor_damage: 0.0,
        hull_max: a(attr::HP).max(1.0),
        hull_damage: 0.0,
        capacitor: Charged::full(a(attr::CAPACITOR_CAPACITY), a(attr::RECHARGE_RATE), now),
        resonance: [
            attr::SHIELD_RESONANCE.map(|r| resonance(t, r)),
            attr::ARMOR_RESONANCE.map(|r| resonance(t, r)),
            attr::HULL_RESONANCE.map(|r| resonance(t, r)),
        ],
    };
    let mut f = Fighter::new(vitals, a(attr::SIGNATURE_RADIUS).max(10.0), a(attr::SCAN_RESOLUTION).max(50.0), lock_range, max_locked);
    f.top_speed = top_speed;
    f.sensors = attr::SENSOR_STRENGTHS.map(a);
    f
}

/// Weapon IDs of an entity, which has no modules to tell its guns apart by.
pub mod slot {
    pub fn gun(ship: i64) -> i64 {
        ship
    }
    pub fn launcher(ship: i64) -> i64 {
        -ship
    }
    pub fn web(ship: i64) -> i64 {
        ship + 1_000_000_000_000
    }
    pub fn scrambler(ship: i64) -> i64 {
        ship + 2_000_000_000_000
    }
}

/// A module template for an entity's weapon.
pub fn entity_module(t: &NpcType, module_id: i64, effect_id: i64, guid: &str, action: Action, duration: f64) -> Active {
    Active {
        module_id,
        type_id: t.type_id,
        effect_id,
        guid: guid.to_string(),
        offensive: true,
        action,
        target: None,
        ammo: None,
        duration_ms: duration.max(500.0) as i64,
        cap_need: 0.0,
        started_ms: 0,
        next_ms: 0,
        cycles: 0,
        repeat: REPEAT_FOREVER,
        stopping: false,
        name: t.name.clone(),
        reach: None,
        landed: true,
    }
}

/// An entity's guns and launchers (and, by chance, its web and scrambler), as
/// module templates aimed later.
pub fn weapons(t: &NpcType, missiles: &HashMap<i64, NpcType>, ship_id: i64, rng: &mut combat::Rng) -> Vec<Active> {
    let mut out = Vec::new();
    let template = |module_id: i64, effect_id: i64, guid: &str, action: Action, duration: f64| {
        entity_module(t, module_id, effect_id, guid, action, duration)
    };
    let multiplier = t.get(attr::DAMAGE_MULTIPLIER);
    if multiplier > 0.0 && t.get(attr::SPEED) > 0.0 {
        let damage = attr::DAMAGE_TYPES.map(|d| t.get(d) * multiplier);
        if damage.iter().sum::<f64>() > 0.0 {
            let gun = Gun {
                optimal: t.get(attr::MAX_RANGE),
                falloff: t.get(attr::FALLOFF),
                tracking: t.get(attr::TRACKING_SPEED),
                optimal_sig: t.get(attr::OPTIMAL_SIG_RADIUS),
            };
            out.push(template(slot::gun(ship_id), EFFECT_TARGET_ATTACK, GUID_TURRET, Action::Turret { damage, gun }, t.get(attr::SPEED)));
        }
    }
    let missile_type = t.get(attr::ENTITY_MISSILE_TYPE_ID) as i64;
    if let Some(m) = missiles.get(&missile_type) {
        let multiplier = if t.get(attr::MISSILE_DAMAGE_MULTIPLIER) > 0.0 { t.get(attr::MISSILE_DAMAGE_MULTIPLIER) } else { 1.0 };
        let damage = attr::DAMAGE_TYPES.map(|d| m.get(d) * multiplier);
        let scale = |a: i32| if t.get(a) > 0.0 { t.get(a) } else { 1.0 };
        let velocity = m.get(attr::MAX_VELOCITY) * scale(MISSILE_VELOCITY_MULTIPLIER);
        let flight_ms = m.get(attr::EXPLOSION_DELAY) * scale(MISSILE_FLIGHT_MULTIPLIER);
        let rate = t.get(attr::MISSILE_LAUNCH_DURATION);
        let rate = if rate >= 1_000.0 { rate } else { 10_000.0 };
        if damage.iter().sum::<f64>() > 0.0 && velocity > 0.0 {
            let action = Action::Missile {
                damage,
                flight_ms: flight_ms as i64,
                range: velocity * flight_ms / 1_000.0,
                cloud: m.get(attr::AOE_CLOUD_SIZE) * scale(MISSILE_CLOUD_MULTIPLIER),
                aoe_velocity: m.get(attr::AOE_VELOCITY) * scale(MISSILE_AOE_VELOCITY_MULTIPLIER),
            };
            let mut launcher = template(slot::launcher(ship_id), EFFECT_MISSILE_LAUNCHING, GUID_MISSILE, action, rate);
            launcher.name = m.name.clone();
            launcher.type_id = m.type_id;
            out.push(launcher);
        }
    }
    // Some carry a web or a scrambler; whether this one does is luck.
    let web_chance = t.get(attr::MODIFY_TARGET_SPEED_CHANCE);
    let web_range = t.get(attr::MODIFY_TARGET_SPEED_RANGE);
    if web_chance > 0.0 && web_range > 0.0 && rng.unit() < web_chance {
        let factor = if t.get(attr::SPEED_FACTOR) < 0.0 { 1.0 + t.get(attr::SPEED_FACTOR) / 100.0 } else { 0.5 };
        let duration = t.get(attr::MODIFY_TARGET_SPEED_DURATION).max(5_000.0);
        let mut web = template(slot::web(ship_id), EFFECT_MODIFY_TARGET_SPEED, GUID_WEB, Action::Web(factor.max(0.05)), duration);
        web.reach = Some((web_range, 0.0));
        out.push(web);
    }
    let scram_range = t.get(attr::WARP_SCRAMBLE_RANGE);
    if t.get(attr::ENTITY_WARP_SCRAMBLE_CHANCE) > 0.0 && scram_range > 0.0 && rng.unit() < t.get(attr::ENTITY_WARP_SCRAMBLE_CHANCE) {
        let mut scram = template(slot::scrambler(ship_id), EFFECT_WARP_SCRAMBLE_FOR_ENTITY, GUID_SCRAMBLE, Action::Scramble(1.0), 5_000.0);
        scram.reach = Some((scram_range, 0.0));
        out.push(scram);
    }
    out
}

const EFFECT_MODIFY_TARGET_SPEED: i64 = 575;
const EFFECT_WARP_SCRAMBLE_FOR_ENTITY: i64 = 563;
const GUID_WEB: &str = "effects.ModifyTargetSpeed";
const GUID_SCRAMBLE: &str = "effects.WarpScramble";

const EFFECT_TARGET_ATTACK: i64 = 10;
const EFFECT_MISSILE_LAUNCHING: i64 = 569;
const GUID_TURRET: &str = "effects.Laser";
const GUID_MISSILE: &str = "effects.MissileDeployment";
const MISSILE_VELOCITY_MULTIPLIER: i32 = 645;
const MISSILE_FLIGHT_MULTIPLIER: i32 = 646;
const MISSILE_CLOUD_MULTIPLIER: i32 = 858;
const MISSILE_AOE_VELOCITY_MULTIPLIER: i32 = 859;

fn think_one(park: &mut Park, id: i64, now: i64, players: &[(i64, Vec3)], out: &mut Out) {
    let Some(rat) = park.ships.get_mut(&id) else { return };
    let (Some(brain), Some(fight)) = (rat.npc.as_mut(), rat.fight.as_mut()) else { return };
    brain.next_think_ms = now + THINK_MS;

    // Patch up between fights, and during them.
    for (heal, armor) in [(&mut brain.shield_boost, false), (&mut brain.armor_repair, true)] {
        let Some(h) = heal.as_mut() else { continue };
        let hurt = if armor {
            fight.vitals.armor_damage > 0.25 * fight.vitals.armor_max
        } else {
            fight.vitals.shield.fraction(now) < 0.75
        };
        if hurt && now >= h.next_ms {
            if armor {
                fight.vitals.repair_armor(h.amount);
            } else {
                fight.vitals.shield.add(now, h.amount);
            }
            h.next_ms = now + h.every_ms;
            out.hurt(id);
        }
    }

    let pos = rat.pos;
    let belt_pos = park.belts.iter().find(|b| b.id == brain.belt).map_or(pos, |b| b.pos);
    let leash = brain.aggro_range().max(LEASH);

    // Is the current target still worth it?
    if let Some(t) = brain.target {
        let keep = players
            .iter()
            .find(|(p, _)| *p == t)
            .is_some_and(|(_, tp)| (*tp - pos).len() <= leash && (*tp - belt_pos).len() <= LEASH * 1.5);
        if !keep {
            disengage(park, id, now, out);
            return;
        }
    }

    let Some(rat) = park.ships.get(&id) else { return };
    let Some(brain) = rat.npc.as_ref() else { return };
    let target = match brain.target {
        Some(t) => t,
        None => {
            let aggro = brain.aggro_range();
            let nearest = players
                .iter()
                .map(|(p, tp)| (*p, (*tp - pos).len()))
                .filter(|(_, d)| *d <= aggro)
                .min_by(|a, b| a.1.total_cmp(&b.1));
            let Some((t, _)) = nearest else { return };
            t
        }
    };
    let signature = park.ships.get(&target).and_then(|s| s.fight.as_ref()).map_or(35.0, |f| f.signature);
    let distance = park.surface_distance(id, target).unwrap_or(f64::MAX);

    // Lock it.
    let Some(rat) = park.ships.get_mut(&id) else { return };
    let (Some(brain), Some(fight)) = (rat.npc.as_mut(), rat.fight.as_mut()) else { return };
    brain.target = Some(target);
    if !fight.locks.contains_key(&target) {
        fight.locks.clear();
        let lock_ms = (combat::lock_seconds(fight.scan_resolution, signature) * 1_000.0) as i64;
        fight.locks.insert(target, Lock::Pending(now + lock_ms));
        debug!("rat {id} locking {target} ({lock_ms} ms)");
    }

    // Close in at full speed, then orbit at its preferred range.
    let chase = distance > brain.attack_range;
    let orbit = Command::Orbit { target, range: brain.fly_range };
    let cruise = brain.cruise;
    let course_changed = rat.command != orbit || brain.chasing != chase;
    brain.chasing = chase;
    if course_changed {
        park.command(id, orbit);
        park.set_speed_fraction(id, if chase { 1.0 } else { cruise });
        let tick = park.tick;
        if let Some(rat) = park.ships.get(&id) {
            for e in space::course_entries(rat, tick) {
                out.destiny(park.solar_system_id, e);
            }
        }
    }

    // Once locked, open fire.
    let Some(rat) = park.ships.get_mut(&id) else { return };
    let (Some(brain), Some(fight)) = (rat.npc.as_mut(), rat.fight.as_mut()) else { return };
    if !fight.is_locked(target) {
        return;
    }
    let mut started = Vec::new();
    for w in &brain.weapons {
        match fight.module_mut(w.module_id) {
            Some(running) => running.target = Some(target),
            None => {
                let mut m = w.clone();
                m.target = Some(target);
                m.started_ms = now;
                // Not everyone fires the instant they lock.
                m.next_ms = now + park.rng.range(0.0, m.duration_ms as f64 * 0.5) as i64;
                started.push(m.clone());
                fight.modules.push(m);
            }
        }
    }
    let tick = park.tick;
    for m in started {
        out.destiny(park.solar_system_id, crate::fight::module_fx(tick, id, &m, true));
    }
}

/// Stop fighting and go back to the belt.
fn disengage(park: &mut Park, id: i64, now: i64, out: &mut Out) {
    let modules: Vec<i64> = park
        .ships
        .get(&id)
        .and_then(|s| s.fight.as_ref())
        .map(|f| f.modules.iter().map(|m| m.module_id).collect())
        .unwrap_or_default();
    for m in modules {
        crate::fight::stop(park, id, m, now, out, None);
    }
    let Some(rat) = park.ships.get_mut(&id) else { return };
    let (Some(brain), Some(fight)) = (rat.npc.as_mut(), rat.fight.as_mut()) else { return };
    let dropped: Vec<(i64, Lock)> = std::mem::take(&mut fight.locks).into_iter().collect();
    brain.target = None;
    brain.chasing = false;
    let home = Command::Orbit { target: brain.belt, range: brain.fly_range.max(5_000.0) };
    let cruise = brain.cruise;
    for (t, lock) in dropped {
        if lock == Lock::Locked {
            crate::fight::tell_target_lost(park, id, t, out);
        }
    }
    park.command(id, home);
    park.set_speed_fraction(id, cruise);
    if let Some(rat) = park.ships.get(&id) {
        for e in space::course_entries(rat, park.tick) {
            out.destiny(park.solar_system_id, e);
        }
    }
    debug!("rat {id} returns to its belt");
}

/// A rat died: its belt starts the clock on a new spawn once it is empty.
pub fn killed(park: &mut Park, ship: &Ship, now: i64) {
    let Some(brain) = &ship.npc else { return };
    let Some(belt) = park.belts.iter_mut().find(|b| b.id == brain.belt) else { return };
    belt.rats.retain(|r| *r != ship.id);
    if belt.rats.is_empty() {
        let wait = park.rng.range(RESPAWN_MS.0, RESPAWN_MS.1) as i64;
        belt.respawn_at = Some(now + wait);
        info!("belt {} cleared; rats return in {}s", belt.id, wait / 1_000);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_sec_spawns_frigates_and_null_sec_spawns_battleships() {
        let mut rng = combat::Rng::seeded(7);
        for _ in 0..20 {
            assert!(plan(0.9, &mut rng).iter().all(|c| *c == Class::Frigate));
            assert!(plan(-0.8, &mut rng).contains(&Class::Battleship));
        }
    }

    #[test]
    fn tiers_pick_weak_rats_in_high_sec_and_strong_ones_in_null() {
        assert_eq!(tier(0.9, 9), 0..3);
        assert_eq!(tier(0.3, 9), 3..6);
        assert_eq!(tier(-0.5, 9), 6..9);
        assert_eq!(tier(0.9, 1), 0..1);
        assert_eq!(tier(-0.5, 2), 1..2);
    }

    #[test]
    fn pirates_follow_the_faction() {
        assert_eq!(pirates_for(Some(500001), 1).corp_id, GURISTAS.corp_id);
        assert_eq!(pirates_for(Some(500004), 1).corp_id, SERPENTIS.corp_id);
        assert_eq!(pirates_for(None, 30002780).corp_id, ALL_PIRATES[(30002780 % 5) as usize].corp_id);
    }

    use crate::ballpark::Celestial;
    use crate::heartbeat::Job;

    const CLIENT: i64 = 77;
    const PLAYER: i64 = 140_000_100;

    fn rat_type() -> NpcType {
        let attrs = [
            (attr::HP, 300.0),
            (attr::ARMOR_HP, 200.0),
            (attr::SHIELD_CAPACITY, 200.0),
            (attr::SHIELD_RECHARGE_RATE, 1_000_000.0),
            (attr::MAX_VELOCITY, 1_000.0),
            (attr::ENTITY_CRUISE_SPEED, 300.0),
            (attr::SPEED, 3_000.0),
            (attr::DAMAGE_MULTIPLIER, 1.0),
            (attr::EM_DAMAGE, 10.0),
            (attr::THERMAL_DAMAGE, 10.0),
            (attr::MAX_RANGE, 5_000.0),
            (attr::FALLOFF, 5_000.0),
            (attr::TRACKING_SPEED, 1.0),
            (attr::ENTITY_ATTACK_RANGE, 10_000.0),
            (attr::ENTITY_FLY_RANGE, 2_000.0),
            (attr::ENTITY_KILL_BOUNTY, 7_500.0),
            (attr::SIGNATURE_RADIUS, 40.0),
            (attr::SCAN_RESOLUTION, 800.0),
        ];
        NpcType {
            type_id: 10013,
            group_id: 550,
            name: "Angel Hijacker".into(),
            radius: 30.0,
            mass: 1_000_000.0,
            attrs: attrs.into_iter().collect(),
        }
    }

    fn player_ship(pos: Vec3) -> Ship {
        let pilot = Pilot {
            type_id: 587,
            group_id: 25,
            name: "Rifter".into(),
            char_id: PLAYER,
            corp_id: 1000002,
            alliance_id: None,
            security: 0.0,
            bounty: 0.0,
        };
        let mut ship = Ship::new(PLAYER + 1, CLIENT, ShipStats::new(30.0, 1_000_000.0, 3.0, 300.0, 2.0), pilot, pos, Vec3::ZERO);
        let vitals = Vitals {
            shield: Charged::full(400.0, 500_000.0, 0),
            armor_max: 400.0,
            armor_damage: 0.0,
            hull_max: 400.0,
            hull_damage: 0.0,
            capacitor: Charged::full(250.0, 150_000.0, 0),
            resonance: [[1.0; 4]; 3],
        };
        let mut fighter = Fighter::new(vitals, 35.0, 600.0, 30_000.0, 3);
        fighter.top_speed = 300.0;
        fighter.sensors = [0.0, 8.0, 0.0, 0.0];
        ship.fight = Some(fighter);
        ship
    }

    fn belt_park() -> Park {
        let belt = Celestial {
            id: 40_000_001,
            type_id: 15,
            group_id: GROUP_BELT,
            name: "Belt".into(),
            pos: Vec3::new(1.0e9, 0.0, 0.0),
            radius: 1.0,
            jump: None,
        };
        let mut park = Park::new(30_000_001, vec![belt], 0);
        let mut wildlife = Wildlife { security: 0.9, corp_id: ANGELS.corp_id, ..Default::default() };
        wildlife.rats[0].push(rat_type());
        wildlife.ores.push(Ore { type_id: 1230, name: "Veldspar".into(), volume: 0.1 });
        park.wildlife = std::sync::Arc::new(wildlife);
        populate(&mut park);
        park
    }

    /// Runs the park for `seconds`, a pulse every 250 ms.
    fn run(park: &mut Park, from_ms: i64, seconds: i64) -> Out {
        let mut out = Out::default();
        for step in 0..seconds * 4 {
            let now = from_ms + step * 250;
            park.advance(now / 1_000);
            crate::fight::step(park, now, &mut out);
        }
        out
    }

    #[test]
    fn rats_spawn_for_a_visitor_lock_and_open_fire() {
        let mut park = belt_park();
        let belt = park.belts[0].pos;
        park.ships.insert(PLAYER + 1, player_ship(belt + Vec3::new(5_000.0, 0.0, 0.0)));
        let out = run(&mut park, 0, 40);

        let rats: Vec<&Ship> = park.ships.values().filter(|s| s.npc.is_some()).collect();
        assert!(!rats.is_empty(), "a belt with a player at it has rats");
        assert!(rats.iter().all(|r| r.npc.as_ref().unwrap().target == Some(PLAYER + 1)));
        let me = park.ships[&(PLAYER + 1)].fight.as_ref().unwrap();
        let hp = me.vitals.shield.at(40_000) + (me.vitals.armor_max - me.vitals.armor_damage);
        assert!(hp < 800.0, "the rats hurt: {hp} of 800 left");
        let told = |event: &str| out.notes().iter().filter(|(c, e, _)| *c == CLIENT && *e == event).count();
        assert!(told("OnTarget") >= 1, "the pilot heard it was locked");
        assert!(told("OnDamageMessage") >= 1, "the pilot heard the hits");
        assert!(told("OnModuleAttributeChanges") >= 1, "the pilot's gauges moved");
        assert!(park.rocks.len() >= 8, "and the belt has asteroids");
    }

    #[test]
    fn a_rat_shot_to_death_pays_its_bounty() {
        let mut park = belt_park();
        let belt = park.belts[0].pos;
        park.ships.insert(PLAYER + 1, player_ship(belt + Vec3::new(5_000.0, 0.0, 0.0)));
        run(&mut park, 0, 1);
        let rat = *park.ships.keys().find(|id| park.ships[id].npc.is_some()).expect("spawned");

        // A big gun, already locked on.
        let gun = Active {
            module_id: 999,
            type_id: 484,
            effect_id: 34,
            guid: "effects.ProjectileFired".into(),
            offensive: true,
            action: Action::Turret {
                damage: [0.0, 0.0, 400.0, 0.0],
                gun: Gun { optimal: 1.0e6, falloff: 1.0, tracking: 100.0, optimal_sig: 1.0 },
            },
            target: Some(rat),
            ammo: None,
            duration_ms: 1_000,
            cap_need: 1.0,
            started_ms: 1_000,
            next_ms: 1_000,
            cycles: 0,
            repeat: REPEAT_FOREVER,
            stopping: false,
            name: "125mm Gatling AutoCannon I".into(),
            reach: None,
            landed: true,
        };
        let me = park.ships.get_mut(&(PLAYER + 1)).unwrap().fight.as_mut().unwrap();
        me.locks.insert(rat, Lock::Locked);
        me.modules.push(gun);
        let out = run(&mut park, 1_000, 20);

        assert!(!park.ships.contains_key(&rat), "the rat is gone");
        let paid: Vec<&Job> = out.jobs.iter().filter(|j| matches!(j, Job::Bounty { .. })).collect();
        assert!(
            paid.iter().any(|j| matches!(j, Job::Bounty { char_id: PLAYER, amount, .. } if *amount == 7_500.0)),
            "{paid:?}"
        );
        let opcodes: Vec<String> = out
            .destiny_entries()
            .iter()
            .filter_map(|(_, e)| match e {
                Value::Tuple(t) => match &t[1] {
                    Value::Tuple(ev) => ev[0].as_string(),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let boom = opcodes.iter().position(|o| o == "TerminalExplosion").expect("it exploded");
        assert_eq!(opcodes[boom + 1], "RemoveBalls", "and was removed in the same update");
        let me = park.ships[&(PLAYER + 1)].fight.as_ref().unwrap();
        assert!(me.modules.iter().all(|m| m.target != Some(rat)), "the gun stopped when its target died");
    }

    /// A module on `on`, already cycling at `target`.
    fn running(module_id: i64, action: Action, target: i64, reach: f64) -> Active {
        Active {
            module_id,
            type_id: 526,
            effect_id: 586,
            guid: "effects.ModifyTargetSpeed".into(),
            offensive: true,
            action,
            target: Some(target),
            ammo: None,
            duration_ms: 5_000,
            cap_need: 0.0,
            started_ms: 0,
            next_ms: 0,
            cycles: 0,
            repeat: REPEAT_FOREVER,
            stopping: false,
            name: "Stasis Webifier I".into(),
            reach: Some((reach, 0.0)),
            landed: false,
        }
    }

    fn second_pilot(park: &mut Park, pos: Vec3) -> i64 {
        let mut other = player_ship(pos);
        other.id = PLAYER + 11;
        other.client_id = CLIENT + 1;
        other.pilot.char_id = PLAYER + 10;
        park.ships.insert(other.id, other);
        PLAYER + 11
    }

    #[test]
    fn a_web_slows_and_a_scrambler_stops_a_warp() {
        let mut park = Park::new(30_000_002, Vec::new(), 0);
        park.wildlife = std::sync::Arc::new(Wildlife { security: -0.2, ..Default::default() });
        park.ships.insert(PLAYER + 1, player_ship(Vec3::ZERO));
        let victim = second_pilot(&mut park, Vec3::new(5_000.0, 0.0, 0.0));
        let me = park.ships.get_mut(&(PLAYER + 1)).unwrap().fight.as_mut().unwrap();
        me.locks.insert(victim, Lock::Locked);
        me.modules.push(running(1, Action::Web(0.25), victim, 10_000.0));
        me.modules.push(running(2, Action::Scramble(2.0), victim, 10_000.0));

        park.command(victim, Command::Warp { target: Vec3::new(1.0e12, 0.0, 0.0), range: 0.0 });
        let out = run(&mut park, 0, 2);
        let v = &park.ships[&victim];
        assert!((v.stats.max_velocity - 75.0).abs() < 1e-6, "webbed to {}", v.stats.max_velocity);
        assert_eq!(v.command, Command::Stop, "the warp was stopped");
        assert!(v.fight.as_ref().unwrap().afflictions.scrambled >= 1.0);
        let scrambled = |a: &Value| a.seq().and_then(|s| s.first()) == Some(&Value::bytes("WarpScrambled"));
        assert!(out.notes().iter().any(|(c, e, a)| *c == CLIENT + 1 && *e == "OnRemoteMessage" && scrambled(a)));
        assert!(out.notes().iter().any(|(_, e, _)| *e == "OnAggressionChange"), "the system hears of the aggression");
    }

    #[test]
    fn a_crime_in_high_sec_brings_concord() {
        let mut park = Park::new(30_000_003, Vec::new(), 0);
        let mut cop = rat_type();
        cop.type_id = 3885;
        cop.group_id = 301;
        cop.name = "CONCORD Police Captain".into();
        park.wildlife = std::sync::Arc::new(Wildlife { security: 1.0, police: vec![cop], ..Default::default() });
        park.ships.insert(PLAYER + 1, player_ship(Vec3::ZERO));
        let victim = second_pilot(&mut park, Vec3::new(2_000.0, 0.0, 0.0));
        let me = park.ships.get_mut(&(PLAYER + 1)).unwrap().fight.as_mut().unwrap();
        me.locks.insert(victim, Lock::Locked);
        me.modules.push(running(1, Action::Web(0.5), victim, 10_000.0));

        let out = run(&mut park, 0, 30);
        let security_hits = out.jobs.iter().filter(|j| matches!(j, Job::Security { char_id: PLAYER, .. })).count();
        assert_eq!(security_hits, 1, "one crime, one penalty");
        assert!(!park.ships.contains_key(&(PLAYER + 1)), "CONCORD destroyed the criminal's ship");
        assert!(out.jobs.iter().any(|j| matches!(j, Job::Lost(l) if l.char_id == PLAYER)));
        assert!(park.ships.contains_key(&victim), "and left the victim alone");
    }

    #[test]
    fn bigger_asteroids_hold_more() {
        assert!(rock_radius(10_000.0) > rock_radius(1_000.0));
        assert!(rock_radius(1e12) <= 12_000.0);
    }
}
