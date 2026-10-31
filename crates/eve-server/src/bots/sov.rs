//! Null-sec sovereignty for bot alliances: they hold space as players do,
//! with control towers that claim it (svc::starbase::sovereignty decides).
//!
//! - Settling (`establish`, once): each null-sec alliance whose home region
//!   no NPC faction holds anchors a large control tower of its race at a moon
//!   in each of its systems, a block spreading out from its home, one system
//!   for about every 25 pilots. Their claims are dated back past the claim
//!   delay, so the map shows the space held from the start.
//! - Upkeep (`upkeep`, every politics tick): towers are kept in 30 days of
//!   fuel and some strontium. Neighbours at war plan sieges of each other's
//!   border systems, announced in the news `SIEGE_NOTICE` ahead
//!   (`battle` fights them). A siege that succeeds (`conclude`) destroys the
//!   defender's towers there and anchors the attacker's, whose claim then
//!   stands its delay (scaled by the bots' clock) before the system changes
//!   hands. An alliance short of systems at home anchors in unclaimed ones.
//!
//! Bot towers are real: players can see them, shoot them and reinforce them.

use std::collections::{HashMap, HashSet, VecDeque};

use tokio_postgres::Client;
use tracing::info;

use super::behaviour::Atlas;
use super::politics::{Politics, KIND_NULL_ALLIANCE};
use crate::combat::Rng;
use crate::svc::rows::{float, int, text};

/// One system for this many pilots, at least `MIN_SYSTEMS`.
const PILOTS_PER_SYSTEM: u32 = 25;
const MIN_SYSTEMS: usize = 3;
/// At most this share of a region's systems goes to its alliance.
const MAX_SHARE: f64 = 0.6;
/// Large control towers by race (Caldari, Minmatar, Amarr, Gallente).
const TOWERS: [(i64, i32); 4] = [(1, 16213), (2, 16214), (4, 12235), (8, 12236)];
const GROUP_MOON: i32 = 8;
/// A tower stands this far off its moon's surface (the moon rule is 100 km).
const OFF_MOON: f64 = 30_000.0;
/// Fuel held: 30 days of hourly periods, topped up below a quarter of that.
const FUEL_PERIODS: i64 = 30 * 24;
const STRONTIUM: i64 = 3_000;
const PURPOSE_ONLINE: i32 = 1;
const PURPOSE_REINFORCE: i32 = 4;
/// Each war's chance, per politics tick, of a siege.
const SIEGE_CHANCE: f64 = 0.03;
/// How long before a siege it is announced, in seconds of the bots' clock.
pub const SIEGE_NOTICE: u32 = 15 * 60;
/// An alliance's chance, per tick, to anchor in an unclaimed home system.
const EXPAND_CHANCE: f64 = 0.05;
const SECOND_FT: i64 = 10_000_000;

/// What happened, for the news.
pub type News = (i64, String);

/// A siege planned or under way: who, where, and when it begins.
pub struct Siege {
    pub war: i32,
    pub attacker: i32,
    pub defender: i32,
    /// The attacker's executor corporation and its race, for its tower.
    pub attacker_exec: i32,
    pub attacker_race: i64,
    /// Pilots in each alliance, for the odds when nobody watches.
    pub attackers: u32,
    pub defenders: u32,
    pub system: i32,
    pub names: SiegeNames,
    /// When it begins: on the bots' clock, and as FILETIME for the dashboard.
    pub starts: u32,
    pub starts_at: i64,
    /// The fleets, once it is fought where someone watches.
    pub fight: Option<super::battle::Fight>,
}

#[derive(Debug, Clone)]
pub struct SiegeNames {
    pub attacker: String,
    pub defender: String,
    pub system: String,
    pub region: String,
    pub security: f64,
}

struct Place {
    system: i32,
    moon: i32,
    pos: (f64, f64, f64),
}

/// The bot alliances' null-sec alliances, with their home region (only those
/// no NPC faction holds), executor corporation, race and pilots.
async fn claimants(db: &Client, p: &Politics) -> Result<Vec<(i32, i32, i32, i64, u32)>, tokio_postgres::Error> {
    let mut out = Vec::new();
    for (id, a) in p.alliances.iter().filter(|(_, a)| a.kind == KIND_NULL_ALLIANCE) {
        let Some(region) = a.region else { continue };
        let sql = r#"SELECT al."executorCorpID", co."raceID" FROM alliances al
                       JOIN "corporation" co ON co."corporationID" = al."executorCorpID"
                       JOIN "mapRegions" r ON r."regionID" = $2
                      WHERE al."allianceID" = $1 AND r."factionID" IS NULL"#;
        let Some(r) = db.query_opt(sql, &[id, &region]).await? else { continue };
        let pilots: u32 = p.corps.values().filter(|c| c.alliance == Some(*id)).map(|c| c.members).sum();
        out.push((*id, region, int(&r, 0).unwrap_or(0) as i32, int(&r, 1).unwrap_or(4), pilots));
    }
    out.sort_unstable();
    Ok(out)
}

/// A moon in the system to put a tower beside, and where.
async fn spot(db: &Client, system: i32, rng: &mut Rng) -> Result<Option<Place>, tokio_postgres::Error> {
    let sql = r#"SELECT "itemID", "x", "y", "z", COALESCE("radius", 1000) FROM "mapDenormalize" WHERE "solarSystemID" = $1 AND "groupID" = $2 ORDER BY "itemID""#;
    let moons = db.query(sql, &[&system, &GROUP_MOON]).await?;
    let Some(m) = moons.get(rng.below(moons.len().max(1))) else { return Ok(None) };
    let dir = rng.in_sphere(1.0);
    let dir = if dir.len() > 1e-6 { dir.unit() } else { crate::ballpark::Vec3::new(1.0, 0.0, 0.0) };
    let reach = float(m, 4).unwrap_or(1_000.0) + OFF_MOON;
    let at = (float(m, 1).unwrap_or(0.0) + dir.x * reach, float(m, 2).unwrap_or(0.0) + dir.y * reach, float(m, 3).unwrap_or(0.0) + dir.z * reach);
    Ok(Some(Place { system, moon: int(m, 0).unwrap_or(0) as i32, pos: at }))
}

/// Anchors and onlines a claiming tower; its claim counts from `claim_since`.
async fn anchor(db: &Client, owner: i32, tower_type: i32, place: &Place, claim_since: i64) -> Result<i32, tokio_postgres::Error> {
    let now = crate::bulkdata::filetime_now();
    let id: i32 = db
        .query_one(
            r#"WITH t AS (INSERT INTO entity ("itemID","itemName","typeID","ownerID","locationID","flag","contraband","singleton","quantity","x","y","z")
                          VALUES (nextval('dyn_id_seq'), '', $1, $2, $3, 0, 0, 1, 1, $4, $5, $6) RETURNING "itemID")
               INSERT INTO "posStructures" ("itemID","state","stateTimestamp","moonID","lastFuel","claim","claimSince","allowCorp","allowAlliance")
               SELECT "itemID", 4, $7, $8, $7, 1, $9, 1, 1 FROM t RETURNING "itemID""#,
            &[&tower_type, &owner, &place.system, &place.pos.0, &place.pos.1, &place.pos.2, &now, &place.moon, &claim_since],
        )
        .await?
        .get(0);
    Ok(id)
}

/// Settles null-sec: towers for every null-sec bot alliance with none yet.
pub async fn establish(db: &Client, atlas: &Atlas, p: &Politics, rng: &mut Rng, claim_delay_secs: i64) -> Result<Vec<News>, tokio_postgres::Error> {
    let have: HashSet<i64> = db
        .query(
            r#"SELECT DISTINCT co."allianceID" FROM "posStructures" p JOIN entity e ON e."itemID" = p."itemID"
                 JOIN "corporation" co ON co."corporationID" = e."ownerID"
                 JOIN "botGroups" g ON g."ownerID" = co."corporationID""#,
            &[],
        )
        .await?
        .iter()
        .filter_map(|r| int(r, 0))
        .collect();
    let now = crate::bulkdata::filetime_now();
    let settled = now - (claim_delay_secs + 60) * SECOND_FT;
    let taken: HashSet<i32> = db
        .query(r#"SELECT "solarSystemID" FROM "mapSolarSystems" WHERE "allianceID" IS NOT NULL"#, &[])
        .await?
        .iter()
        .filter_map(|r| int(r, 0).map(|v| v as i32))
        .collect();
    let mut claimed: HashSet<i32> = taken;
    let mut towers = 0;
    let mut news = Vec::new();
    for (alliance, region, executor, race, pilots) in claimants(db, p).await? {
        if have.contains(&(alliance as i64)) {
            continue;
        }
        let in_region: Vec<i32> = atlas.systems.iter().filter(|(_, s)| s.region == region && s.security <= 0.0).map(|(id, _)| *id).collect();
        let want = ((pilots / PILOTS_PER_SYSTEM) as usize).max(MIN_SYSTEMS).min((in_region.len() as f64 * MAX_SHARE) as usize).max(1);
        let tower_type = TOWERS.iter().find(|(r, _)| *r == race).map_or(TOWERS[2].1, |(_, t)| *t);
        // Outward from home: the region's station systems first, then by gates.
        let home = in_region.iter().copied().find(|s| atlas.systems.get(s).is_some_and(|x| !x.stations.is_empty())).or_else(|| in_region.first().copied());
        let Some(home) = home else { continue };
        let mut order: Vec<i32> = Vec::new();
        let mut seen = HashSet::from([home]);
        let mut queue = VecDeque::from([home]);
        while let Some(s) = queue.pop_front() {
            order.push(s);
            for n in atlas.systems.get(&s).map(|x| x.neighbours.clone()).unwrap_or_default() {
                if atlas.systems.get(&n).is_some_and(|x| x.region == region && x.security <= 0.0) && seen.insert(n) {
                    queue.push_back(n);
                }
            }
        }
        let mut held = 0;
        for system in order {
            if held >= want {
                break;
            }
            if claimed.contains(&system) {
                continue;
            }
            let Some(place) = spot(db, system, rng).await? else { continue };
            anchor(db, executor, tower_type, &place, settled).await?;
            claimed.insert(system);
            held += 1;
            towers += 1;
        }
        let name = alliance_name(db, alliance).await;
        news.push((now, format!("{name} claims {held} systems of {}", region_name(db, region).await)));
    }
    if towers > 0 {
        refuel(db).await?;
        info!("bots: {towers} control towers anchored across null-sec");
    }
    Ok(news)
}

/// Keeps every bot tower in fuel: a top-up whenever one runs below a quarter
/// of 30 days' worth.
pub async fn refuel(db: &Client) -> Result<u64, tokio_postgres::Error> {
    let sql = r#"WITH towers AS (
                     SELECT p."itemID", e."typeID", e."ownerID" FROM "posStructures" p
                       JOIN entity e ON e."itemID" = p."itemID"
                       JOIN "botGroups" g ON g."ownerID" = e."ownerID"
                      WHERE p."controlTowerID" IS NULL AND p."state" IN (2, 3, 4, 5)),
                 needs AS (
                     SELECT t."itemID", t."ownerID", r."resourceTypeID" AS kind,
                            CASE WHEN r."purpose" = $2 THEN $4::bigint ELSE r."quantity"::bigint * $3 END AS target
                       FROM towers t JOIN "invControlTowerResources" r ON r."controlTowerTypeID" = t."typeID"
                      WHERE r."purpose" IN ($1, $2) AND r."minSecurityLevel" IS NULL AND r."factionID" IS NULL),
                 held AS (
                     SELECT e."locationID", e."typeID", sum(e."quantity")::bigint AS q FROM entity e
                      WHERE e."locationID" IN (SELECT "itemID" FROM towers) GROUP BY 1, 2)
               INSERT INTO entity ("itemID","itemName","typeID","ownerID","locationID","flag","contraband","singleton","quantity")
               SELECT nextval('dyn_id_seq'), '', n.kind, n."ownerID", n."itemID", 0, 0, 0, (n.target - COALESCE(h.q, 0))::int
                 FROM needs n LEFT JOIN held h ON h."locationID" = n."itemID" AND h."typeID" = n.kind
                WHERE COALESCE(h.q, 0) * 4 < n.target"#;
    db.execute(sql, &[&PURPOSE_ONLINE, &PURPOSE_REINFORCE, &FUEL_PERIODS, &STRONTIUM]).await
}

/// Every politics tick: fuel, sieges planned between warring neighbours
/// (added to `sieges`), and expansion into empty home systems. Returns the
/// news.
#[allow(clippy::too_many_arguments)]
pub async fn upkeep(
    db: &Client,
    atlas: &Atlas,
    p: &mut Politics,
    watched: &HashSet<i32>,
    sieges: &mut Vec<Siege>,
    rng: &mut Rng,
    clock: u32,
    claim_delay_secs: i64,
    speed: f64,
) -> Result<Vec<News>, tokio_postgres::Error> {
    refuel(db).await?;
    let now = crate::bulkdata::filetime_now();
    // A new claim waits out the delay, run at the bots' speed.
    let delay = claim_delay_secs as f64;
    let claim_since = now - ((delay - delay / speed.max(1.0)) as i64) * SECOND_FT;
    let mut news = Vec::new();
    let claimants = claimants(db, p).await?;
    let info: HashMap<i32, (i32, i32, i64, u32)> = claimants.iter().map(|(a, r, e, race, n)| (*a, (*r, *e, *race, *n))).collect();
    // Which alliances have claiming towers where: what sieges aim at, rather
    // than the map's sovereignty, which follows the towers only later.
    let mut towers: HashMap<i32, HashSet<i32>> = HashMap::new();
    let sql = r#"SELECT e."locationID", COALESCE(co."allianceID", 0) FROM "posStructures" p JOIN entity e ON e."itemID" = p."itemID"
                   LEFT JOIN "corporation" co ON co."corporationID" = e."ownerID" WHERE p."claim" = 1"#;
    for r in db.query(sql, &[]).await? {
        if let (Some(system), Some(alliance)) = (int(&r, 0), int(&r, 1)) {
            towers.entry(system as i32).or_default().insert(alliance as i32);
        }
    }
    let has = |towers: &HashMap<i32, HashSet<i32>>, system: &i32, alliance: i32| towers.get(system).is_some_and(|t| t.contains(&alliance));

    // Sieges: one at a time in each war, announced ahead.
    let wars: Vec<(i32, i32, i32)> = p.wars.iter().map(|w| (w.a, w.b, w.id)).collect();
    for (a, b, war) in wars {
        let (Some(&(_, attacker_exec, attacker_race, attackers)), Some(&(_, _, _, defenders))) = (info.get(&a), info.get(&b)) else { continue };
        if sieges.iter().any(|s| s.war == war) || rng.unit() >= SIEGE_CHANCE {
            continue;
        }
        // A defender's system next to the attacker's space, else any of theirs.
        let mut theirs: Vec<i32> = towers.keys().copied().filter(|s| has(&towers, s, b) && !sieges.iter().any(|x| x.system == *s)).collect();
        theirs.sort_unstable();
        let border: Vec<i32> = theirs
            .iter()
            .copied()
            .filter(|s| atlas.systems.get(s).is_some_and(|x| x.neighbours.iter().any(|nb| has(&towers, nb, a))))
            .collect();
        let pool = if border.is_empty() { &theirs } else { &border };
        let Some(&target) = pool.get(rng.below(pool.len().max(1))) else { continue };
        let names = SiegeNames {
            attacker: alliance_name(db, a).await,
            defender: alliance_name(db, b).await,
            system: system_name(db, target).await,
            region: region_name(db, atlas.systems.get(&target).map_or(0, |x| x.region)).await,
            security: atlas.systems.get(&target).map_or(0.0, |x| x.security as f64),
        };
        let starts_at = now + (SIEGE_NOTICE as f64 / speed.max(0.001)) as i64 * SECOND_FT;
        info!("bots: {} will besiege {} ({}) of {}", names.attacker, names.system, names.region, names.defender);
        news.push((now, format!("{} are massing a fleet to besiege {} in {}, held by {}", names.attacker, names.system, names.region, names.defender)));
        sieges.push(Siege {
            war,
            attacker: a,
            defender: b,
            attacker_exec,
            attacker_race,
            attackers,
            defenders,
            system: target,
            names,
            starts: clock + SIEGE_NOTICE,
            starts_at,
            fight: None,
        });
    }

    // Expansion into empty home systems.
    for (alliance, region, executor, race, pilots) in &claimants {
        if rng.unit() >= EXPAND_CHANCE {
            continue;
        }
        let size = atlas.systems.values().filter(|s| s.region == *region && s.security <= 0.0).count();
        let want = ((*pilots / PILOTS_PER_SYSTEM) as usize).max(MIN_SYSTEMS).min((size as f64 * MAX_SHARE) as usize);
        let have = towers.values().filter(|t| t.contains(alliance)).count();
        if have >= want {
            continue;
        }
        let empty: Vec<i32> = atlas
            .systems
            .iter()
            .filter(|(id, s)| s.region == *region && s.security <= 0.0 && towers.get(id).is_none_or(|t| t.is_empty()) && !watched.contains(id))
            .map(|(id, _)| *id)
            .collect();
        let Some(&system) = empty.get(rng.below(empty.len().max(1))) else { continue };
        let tower_type = TOWERS.iter().find(|(r, _)| r == race).map_or(TOWERS[2].1, |(_, t)| *t);
        if let Some(place) = spot(db, system, rng).await? {
            anchor(db, *executor, tower_type, &place, claim_since).await?;
            towers.entry(system).or_default().insert(*alliance);
            news.push((now, format!("{} anchored a control tower in {}", alliance_name(db, *alliance).await, system_name(db, system).await)));
        }
    }
    Ok(news)
}

/// How a siege ended: `won` by the attacker or not, with `lost` ships on
/// each side. A won siege destroys the defender's towers in the system and
/// anchors the attacker's, its claim to stand the delay (scaled by the bots'
/// clock). Returns the news.
#[allow(clippy::too_many_arguments)]
pub async fn conclude(
    db: &Client,
    p: &mut Politics,
    s: &Siege,
    won: bool,
    lost: (u32, u32),
    rng: &mut Rng,
    claim_delay_secs: i64,
    speed: f64,
) -> Result<News, tokio_postgres::Error> {
    let now = crate::bulkdata::filetime_now();
    if let Some(w) = p.wars.iter_mut().find(|w| w.id == s.war) {
        w.kills += lost.0 + lost.1;
    }
    p.lost += (lost.0 + lost.1) as u64;
    let n = &s.names;
    if !won {
        info!("bots: {} driven back from {} by {} ({} and {} ships lost)", n.attacker, n.system, n.defender, lost.0, lost.1);
        return Ok((now, format!("{} were driven back from {} by {}", n.attacker, n.system, n.defender)));
    }
    let gone = destroy_towers(db, s.system, s.defender).await?;
    let delay = claim_delay_secs as f64;
    let claim_since = now - ((delay - delay / speed.max(1.0)) as i64) * SECOND_FT;
    let tower_type = TOWERS.iter().find(|(r, _)| *r == s.attacker_race).map_or(TOWERS[2].1, |(_, t)| *t);
    if let Some(place) = spot(db, s.system, rng).await? {
        anchor(db, s.attacker_exec, tower_type, &place, claim_since).await?;
        refuel(db).await?;
    }
    info!("bots: {} besieged {}: {gone} tower(s) of {} destroyed", n.attacker, n.system, n.defender);
    Ok((now, format!("{} destroyed {}'s towers in {} and anchored their own", n.attacker, n.defender, n.system)))
}

/// Brings a system's park up to date with its towers after a siege: the
/// destroyed ones explode, the new one appears.
pub async fn show_towers(db: &Client, world: &crate::world::World, system: i32) {
    let loaded = match crate::pos::load(db, system as i64).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("bots: reading the towers of {system}: {}", crate::logging::chain(&e));
            return;
        }
    };
    let mut w = world.lock();
    let Some(park) = w.parks.get_mut(&(system as i64)) else { return };
    park.advance(crate::destiny::tick_now());
    let tick = park.tick;
    let mut entries = Vec::new();
    let gone: Vec<i64> = park.structures.keys().copied().filter(|id| !loaded.iter().any(|s| s.id == *id)).collect();
    for id in gone {
        let Some(s) = park.structures.remove(&id) else { continue };
        entries.push(crate::svc::space::entry(tick, "TerminalExplosion", vec![eve_marshal::Value::Int(id)]));
        let mut balls = vec![eve_marshal::Value::Int(id)];
        if let Some(f) = &s.field {
            balls.push(eve_marshal::Value::Int(f.id));
        }
        entries.push(crate::svc::space::entry(tick, "RemoveBalls", vec![eve_marshal::Value::List(balls)]));
    }
    for s in loaded {
        if !park.structures.contains_key(&s.id) {
            // Its force field goes up with the next pulse (`pos::step`).
            entries.push(crate::pos::add_entry(tick, &s));
            park.structures.insert(s.id, s);
        }
    }
    crate::svc::space::broadcast(&w, system as i64, None, entries);
}

/// Destroys an alliance's towers (and what they hold) in a system.
async fn destroy_towers(db: &Client, system: i32, alliance: i32) -> Result<u64, tokio_postgres::Error> {
    let towers: Vec<i32> = db
        .query(
            r#"SELECT e."itemID" FROM entity e JOIN "posStructures" p ON p."itemID" = e."itemID"
                 JOIN "corporation" co ON co."corporationID" = e."ownerID"
                WHERE e."locationID" = $1 AND co."allianceID" = $2"#,
            &[&system, &alliance],
        )
        .await?
        .iter()
        .filter_map(|r| int(r, 0).map(|v| v as i32))
        .collect();
    if towers.is_empty() {
        return Ok(0);
    }
    db.execute(r#"DELETE FROM entity WHERE "locationID" = ANY($1)"#, &[&towers]).await?;
    db.execute(r#"DELETE FROM "posStructures" WHERE "itemID" = ANY($1) OR "controlTowerID" = ANY($1)"#, &[&towers]).await?;
    db.execute(r#"DELETE FROM entity WHERE "itemID" = ANY($1)"#, &[&towers]).await
}

pub async fn alliance_name(db: &Client, id: i32) -> String {
    let sql = r#"SELECT "allianceName" FROM alliances WHERE "allianceID" = $1"#;
    db.query_opt(sql, &[&id]).await.ok().flatten().and_then(|r| text(&r, 0)).unwrap_or_else(|| id.to_string())
}

pub async fn system_name(db: &Client, id: i32) -> String {
    let sql = r#"SELECT "solarSystemName" FROM "mapSolarSystems" WHERE "solarSystemID" = $1"#;
    db.query_opt(sql, &[&id]).await.ok().flatten().and_then(|r| text(&r, 0)).unwrap_or_else(|| id.to_string())
}

async fn region_name(db: &Client, id: i32) -> String {
    let sql = r#"SELECT "regionName" FROM "mapRegions" WHERE "regionID" = $1"#;
    db.query_opt(sql, &[&id]).await.ok().flatten().and_then(|r| text(&r, 0)).unwrap_or_else(|| id.to_string())
}
