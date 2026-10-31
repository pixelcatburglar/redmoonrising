//! Making the bots and reading them back: the map they live on, and the
//! characters themselves.
//!
//! A bot is a real character on the account rmr-bots: named from the NPC
//! characters' first and last names of its race, with the face of an NPC of
//! its bloodline and gender, in its bloodline's school corporation, with a
//! ship of its race and role in its home station's hangar. Its skills live
//! only in the simulation. It is not a static owner: the client learns its
//! name when it needs it, as for any player (config.StaticOwners goes whole
//! to every client at login, and has a 1 MB ceiling). They are made in batches, each in one
//! transaction of a few set-based inserts.

use std::collections::{HashMap, HashSet};

use tokio_postgres::Client;
use tracing::info;

use super::behaviour::{Activity, Atlas, Bot, Role, System};
use super::presence::Identity;
use crate::combat::Rng;
use crate::svc::rows::{float, int, text};

/// The share of bots in each role.
pub const ROLES: [(Role, f64); 6] = [
    (Role::Miner, 0.30),
    (Role::Trader, 0.15),
    (Role::Hauler, 0.15),
    (Role::Ratter, 0.20),
    (Role::Explorer, 0.10),
    (Role::Pirate, 0.10),
];

/// Jita 4-4, Amarr VIII, Dodixie IX-20, Rens VI-8, Hek VIII-12.
const HUBS: [i32; 5] = [60003760, 60008494, 60011866, 60004588, 60005686];
const BOT_ACCOUNT: &str = "rmr-bots";
const BATCH: usize = 5_000;
const GROUP_BELT: i32 = 9;
const HIGH_SEC: f32 = 0.45;

// ---- the map -------------------------------------------------------------------------------

pub async fn atlas(db: &Client) -> Result<Atlas, String> {
    let fail = |what: &str, e: tokio_postgres::Error| format!("{what}: {}", crate::logging::chain(&e));
    let graph = crate::gates::graph(db).await.ok_or("the star gates could not be read")?;
    let mut systems: HashMap<i32, System> = HashMap::new();
    let sql = r#"SELECT "solarSystemID", "security", "regionID" FROM "mapSolarSystems""#;
    for r in db.query(sql, &[]).await.map_err(|e| fail("reading the systems", e))? {
        let id = int(&r, 0).unwrap_or(0) as i32;
        let neighbours = graph.get(&(id as i64)).map(|n| n.iter().map(|s| *s as i32).collect()).unwrap_or_default();
        let system = System {
            security: float(&r, 1).unwrap_or(0.0) as f32,
            region: int(&r, 2).unwrap_or(0) as i32,
            stations: Vec::new(),
            belts: 0,
            neighbours,
        };
        systems.insert(id, system);
    }
    let mut station_system = HashMap::new();
    let sql = r#"SELECT "stationID", "solarSystemID" FROM "staStations" ORDER BY "stationID""#;
    for r in db.query(sql, &[]).await.map_err(|e| fail("reading the stations", e))? {
        let (station, system) = (int(&r, 0).unwrap_or(0) as i32, int(&r, 1).unwrap_or(0) as i32);
        if let Some(s) = systems.get_mut(&system) {
            s.stations.push(station);
            station_system.insert(station, system);
        }
    }
    let sql = r#"SELECT "solarSystemID", count(*) FROM "mapDenormalize" WHERE "groupID" = $1 GROUP BY 1"#;
    for r in db.query(sql, &[&GROUP_BELT]).await.map_err(|e| fail("reading the belts", e))? {
        if let Some(s) = systems.get_mut(&(int(&r, 0).unwrap_or(0) as i32)) {
            s.belts = int(&r, 1).unwrap_or(0).min(u16::MAX as i64) as u16;
        }
    }
    let belts_near = systems
        .keys()
        .filter_map(|&id| {
            crate::gates::reach_on(graph, id as i64, 3)
                .into_iter()
                .find(|(s, _)| systems.get(&(*s as i32)).is_some_and(|x| x.belts > 0))
                .map(|(s, _)| (id, s as i32))
        })
        .collect();
    let hubs = HUBS.iter().copied().filter(|h| station_system.contains_key(h)).collect();
    Ok(Atlas { systems, station_system, hubs, belts_near, graph })
}

// ---- making them ---------------------------------------------------------------------------

/// What generating needs from the database, read once.
struct Sources {
    /// First and last names by race.
    names: HashMap<i64, (Vec<String>, Vec<String>)>,
    taken: HashSet<String>,
    /// NPC characters to take faces from, by (bloodline, gender).
    faces: HashMap<(i64, i16), Vec<i32>>,
    /// Each player bloodline's race.
    bloodlines: Vec<(i64, i64)>,
    /// Hulls by (group, race).
    hulls: HashMap<(i32, i64), Vec<i32>>,
}

async fn sources(db: &Client) -> Result<Sources, tokio_postgres::Error> {
    let mut names: HashMap<i64, (Vec<String>, Vec<String>)> = HashMap::new();
    let sql = r#"SELECT b."raceID", c."characterName" FROM "character_" c JOIN "chrBloodlines" b ON b."bloodlineID" = c."bloodlineID"
                  WHERE c."accountID" IS NULL AND c."characterName" LIKE '% %'"#;
    for r in db.query(sql, &[]).await? {
        let (race, name) = (int(&r, 0).unwrap_or(0), text(&r, 1).unwrap_or_default());
        let (Some(first), Some(last)) = (name.split(' ').next(), name.rsplit(' ').next()) else { continue };
        let entry = names.entry(race).or_default();
        entry.0.push(first.to_string());
        entry.1.push(last.to_string());
    }
    for (firsts, lasts) in names.values_mut() {
        firsts.sort();
        firsts.dedup();
        lasts.sort();
        lasts.dedup();
    }
    let taken = db
        .query(r#"SELECT lower("characterName") FROM "character_""#, &[])
        .await?
        .iter()
        .filter_map(|r| text(r, 0))
        .collect();
    let mut faces: HashMap<(i64, i16), Vec<i32>> = HashMap::new();
    let sql = r#"SELECT "bloodlineID", "gender", "characterID" FROM "character_" WHERE "accountID" IS NULL AND "bloodlineID" BETWEEN 1 AND 8"#;
    for r in db.query(sql, &[]).await? {
        faces.entry((int(&r, 0).unwrap_or(0), int(&r, 1).unwrap_or(0) as i16)).or_default().push(int(&r, 2).unwrap_or(0) as i32);
    }
    let bloodlines = db
        .query(r#"SELECT "bloodlineID", "raceID" FROM "chrBloodlines" WHERE "bloodlineID" BETWEEN 1 AND 8 ORDER BY 1"#, &[])
        .await?
        .iter()
        .map(|r| (int(r, 0).unwrap_or(0), int(r, 1).unwrap_or(0)))
        .collect();
    let mut hulls: HashMap<(i32, i64), Vec<i32>> = HashMap::new();
    let sql = r#"SELECT "groupID", "raceID", "typeID" FROM "invTypes"
                  WHERE "groupID" = ANY($1) AND "marketGroupID" IS NOT NULL AND "raceID" IS NOT NULL ORDER BY "typeID""#;
    for r in db.query(sql, &[&vec![25i32, 26, 27, 28, 31, 419, 463]]).await? {
        hulls.entry((int(&r, 0).unwrap_or(0) as i32, int(&r, 1).unwrap_or(0))).or_default().push(int(&r, 2).unwrap_or(0) as i32);
    }
    Ok(Sources { names, taken, faces, bloodlines, hulls })
}

/// How likely a role is to live at a station, by where it is.
fn home_weight(role: Role, atlas: &Atlas, station: i32) -> f64 {
    let Some(system) = atlas.station_system.get(&station).and_then(|s| atlas.systems.get(s)) else { return 0.0 };
    let (high, low) = (system.security >= HIGH_SEC, system.security > 0.0 && system.security < HIGH_SEC);
    let hub = atlas.hubs.contains(&station);
    let hub_system = atlas.hubs.iter().any(|h| atlas.station_system.get(h) == atlas.station_system.get(&station));
    match role {
        Role::Miner if system.belts == 0 => 0.05,
        Role::Miner => if high { 1.0 } else if low { 0.25 } else { 0.1 },
        Role::Trader if hub => 400.0,
        Role::Trader if hub_system => 20.0,
        Role::Trader => if high { 0.3 } else { 0.02 },
        Role::Hauler => if hub_system { 10.0 } else if high { 1.0 } else if low { 0.2 } else { 0.05 },
        Role::Ratter => if high { 0.1 } else if low { 1.0 } else { 1.5 },
        Role::Explorer => 1.0,
        Role::Pirate => if low { 1.0 } else if high { 0.0 } else { 0.3 },
    }
}

/// Weighted picking, by cumulative weight.
struct Pick {
    items: Vec<i32>,
    cumulative: Vec<f64>,
}

impl Pick {
    fn new(items: impl IntoIterator<Item = (i32, f64)>) -> Pick {
        let (mut out, mut cumulative, mut total) = (Vec::new(), Vec::new(), 0.0);
        for (item, w) in items.into_iter().filter(|(_, w)| *w > 0.0) {
            total += w;
            out.push(item);
            cumulative.push(total);
        }
        Pick { items: out, cumulative }
    }

    fn pick(&self, rng: &mut Rng) -> i32 {
        let x = rng.unit() * self.cumulative.last().copied().unwrap_or(0.0);
        let i = self.cumulative.partition_point(|c| *c < x).min(self.items.len().saturating_sub(1));
        self.items[i]
    }
}

/// The hull a bot of a role and race flies.
fn hull(role: Role, race: i64, s: &Sources, rng: &mut Rng) -> i32 {
    let groups: &[i32] = match role {
        Role::Miner => &[25, 463],
        Role::Hauler => &[28],
        Role::Trader => &[31, 28],
        Role::Ratter => &[26, 419, 27],
        Role::Explorer => &[25],
        Role::Pirate => &[25, 26],
    };
    let group = groups[rng.below(groups.len())];
    let list = s.hulls.get(&(group, race)).or_else(|| s.hulls.get(&(25, race))).or_else(|| s.hulls.values().next());
    list.map_or(0, |l| l[rng.below(l.len())])
}

fn name(race: i64, s: &mut Sources, rng: &mut Rng) -> Option<String> {
    let races: Vec<i64> = s.names.keys().copied().collect();
    for attempt in 0..40 {
        let r = if attempt < 30 { race } else { races[rng.below(races.len())] };
        let (firsts, lasts) = s.names.get(&r)?;
        let full = format!("{} {}", firsts[rng.below(firsts.len())], lasts[rng.below(lasts.len())]);
        if crate::svc::charcreation::name_shape_ok(&full) && s.taken.insert(full.to_lowercase()) {
            return Some(full);
        }
    }
    None
}

/// Makes bots until there are `target`; returns how many there are.
pub async fn ensure(db: &mut Client, atlas: &Atlas, target: usize) -> Result<usize, String> {
    let fail = |what: &str, e: tokio_postgres::Error| format!("{what}: {}", crate::logging::chain(&e));
    let have: i64 = db.query_one(r#"SELECT count(*) FROM "bots""#, &[]).await.map_err(|e| fail("counting bots", e))?.get(0);
    let have = have as usize;
    if have >= target {
        return Ok(have);
    }
    let account: i32 = db
        .query_one(
            r#"WITH found AS (SELECT "accountID" FROM "account" WHERE "accountName" = $1),
                    made AS (INSERT INTO "account" ("accountID", "accountName", "password", "role", "banned")
                             SELECT (SELECT COALESCE(max("accountID"), 0) + 1 FROM "account"), $1, '', 0, 1
                              WHERE NOT EXISTS (SELECT 1 FROM found) RETURNING "accountID")
               SELECT "accountID" FROM found UNION ALL SELECT "accountID" FROM made"#,
            &[&BOT_ACCOUNT],
        )
        .await
        .map_err(|e| fail("the bot account", e))?
        .get(0);
    let mut src = sources(db).await.map_err(|e| fail("reading names, faces and hulls", e))?;
    let stations: Vec<i32> = atlas.station_system.keys().copied().collect();
    let homes: Vec<(Role, Pick)> = ROLES.iter().map(|(role, _)| (*role, Pick::new(stations.iter().map(|s| (*s, home_weight(*role, atlas, *s)))))).collect();
    let roles = Pick::new(ROLES.iter().map(|(r, w)| (*r as i32, *w)));
    let mut rng = Rng::seeded(crate::bulkdata::filetime_now() as u64);
    let started = std::time::Instant::now();
    let mut made = 0;
    while have + made < target {
        let n = BATCH.min(target - have - made);
        let mut batch = Vec::with_capacity(n);
        for _ in 0..n {
            let role = Role::from_i16(roles.pick(&mut rng) as i16);
            let (bloodline, race) = src.bloodlines[rng.below(src.bloodlines.len())];
            let gender = rng.below(2) as i16;
            let Some(face) = src.faces.get(&(bloodline, gender)).map(|f| f[rng.below(f.len())]) else { continue };
            let Some(name) = name(race, &mut src, &mut rng) else { continue };
            let home = homes.iter().find(|(r, _)| *r == role).map_or(HUBS[0], |(_, p)| p.pick(&mut rng));
            let ship = hull(role, race, &src, &mut rng);
            let balance = 10f64.powf(rng.range(5.5, 8.5)).round();
            let security = if role == Role::Pirate { -rng.range(2.0, 8.0) } else { rng.range(-0.5, 3.0) };
            let share = rng.range(0.1, 0.5) as f32;
            batch.push(NewBot { name, bloodline: bloodline as i32, gender, face, home, ship, balance, security, role, share });
        }
        insert(db, account, &batch).await.map_err(|e| fail("making bots", e))?;
        made += batch.len();
        info!("bots: made {} of {} ({:.0}/s)", have + made, target, made as f64 / started.elapsed().as_secs_f64().max(0.001));
    }
    Ok(have + made)
}

struct NewBot {
    name: String,
    bloodline: i32,
    gender: i16,
    face: i32,
    home: i32,
    ship: i32,
    balance: f64,
    security: f64,
    role: Role,
    share: f32,
}

/// The appearance columns a bot copies from its face.
const LOOKS: &str = r#""accessoryID","beardID","costumeID","decoID","eyebrowsID","eyesID","hairID","lipstickID","makeupID","skinID",
    "backgroundID","lightID","headRotation1","headRotation2","headRotation3","eyeRotation1","eyeRotation2","eyeRotation3",
    "camPos1","camPos2","camPos3","morph1e","morph1n","morph1s","morph1w","morph2e","morph2n","morph2s","morph2w",
    "morph3e","morph3n","morph3s","morph3w","morph4e","morph4n","morph4s","morph4w""#;

async fn insert(db: &mut Client, account: i32, batch: &[NewBot]) -> Result<(), tokio_postgres::Error> {
    let tx = db.transaction().await?;
    let ids: Vec<i32> = tx
        .query("SELECT nextval('dyn_id_seq')::int FROM generate_series(1, $1)", &[&(2 * batch.len() as i32)])
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let (chars, ships) = ids.split_at(batch.len());
    let col = |f: fn(&NewBot) -> i32| batch.iter().map(f).collect::<Vec<i32>>();
    let names: Vec<&str> = batch.iter().map(|b| b.name.as_str()).collect();
    let (bloodline, face, home, ship_type) = (col(|b| b.bloodline), col(|b| b.face), col(|b| b.home), col(|b| b.ship));
    let gender: Vec<i16> = batch.iter().map(|b| b.gender).collect();
    let balance: Vec<f64> = batch.iter().map(|b| b.balance).collect();
    let security: Vec<f64> = batch.iter().map(|b| b.security).collect();
    let role: Vec<i16> = batch.iter().map(|b| b.role as i16).collect();
    let share: Vec<f32> = batch.iter().map(|b| b.share).collect();
    let now = crate::bulkdata::filetime_now();
    let new = r#"unnest($1::int[], $2::text[], $3::int[], $4::smallint[], $5::int[], $6::int[], $7::int[], $8::int[], $9::float8[], $10::float8[])
                   AS u(id, name, bloodline, gender, face, home, ship, ship_type, balance, sec)"#;
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 10] =
        [&chars, &names, &bloodline, &gender, &face, &home, &ships, &ship_type, &balance, &security];
    let character = format!(
        r#"INSERT INTO "character_" ("characterID","characterName","accountID","balance","securityRating","corporationID",
                "corporationDateTime","startDateTime","createDateTime","typeID","raceID","bloodlineID","gender",{LOOKS},
                "stationID","solarSystemID","constellationID","regionID","shipID")
           SELECT u.id, u.name, {account}, u.balance, u.sec, b."corporationID", {now}, {now}, {now}, f."typeID", b."raceID",
                  u.bloodline, u.gender, {face_looks}, s."stationID", s."solarSystemID", s."constellationID", s."regionID", u.ship
             FROM {new}
             JOIN "character_" f ON f."characterID" = u.face
             JOIN "chrBloodlines" b ON b."bloodlineID" = u.bloodline
             JOIN "staStations" s ON s."stationID" = u.home"#,
        face_looks = LOOKS.split(',').map(|c| format!("f.{}", c.trim())).collect::<Vec<_>>().join(",")
    );
    tx.execute(character.as_str(), &params).await?;
    let rest = format!(
        r#"WITH u AS (SELECT * FROM {new}),
                bodies AS (INSERT INTO entity ("itemID","itemName","typeID","ownerID","locationID","flag","contraband","singleton","quantity")
                           SELECT u.id, u.name, c."typeID", u.id, u.home, 0, 0, 1, 1 FROM u JOIN "character_" c ON c."characterID" = u.id),
                hulls AS (INSERT INTO entity ("itemID","itemName","typeID","ownerID","locationID","flag","contraband","singleton","quantity")
                          SELECT u.ship, '', u.ship_type, u.id, u.home, 4, 0, 1, 1 FROM u),
                names AS (INSERT INTO "eveNames" ("itemID","itemName","categoryID","groupID","typeID")
                          SELECT u.id, u.name, 1, 1, c."typeID" FROM u JOIN "character_" c ON c."characterID" = u.id),
                roles AS (INSERT INTO "chrCorporationRoles" ("characterID","corprole","rolesAtAll","rolesAtBase","rolesAtHQ","rolesAtOther")
                          SELECT u.id, 0, 0, 0, 0, 0 FROM u),
                jobs AS (INSERT INTO "chrEmployment" ("characterID","corporationID","startDate","deleted")
                         SELECT u.id, c."corporationID", {now}, 0 FROM u JOIN "character_" c ON c."characterID" = u.id)
           UPDATE "corporation" co SET "memberCount" = co."memberCount" + n.members
             FROM (SELECT b."corporationID", count(*)::int AS members FROM u JOIN "chrBloodlines" b ON b."bloodlineID" = u.bloodline GROUP BY 1) n
            WHERE co."corporationID" = n."corporationID""#
    );
    tx.execute(rest.as_str(), &params).await?;
    tx.execute(
        r#"INSERT INTO "bots" ("characterID","role","homeStationID","share") SELECT * FROM unnest($1::int[], $2::smallint[], $3::int[], $4::real[])"#,
        &[&chars, &role, &home, &share],
    )
    .await?;
    tx.commit().await
}

// ---- reading them back ---------------------------------------------------------------------

/// Up to `limit` bots, each docked where it was (or at home, if it was in
/// space when the server stopped), offline; with who each is.
pub async fn load(db: &Client, atlas: &Atlas, limit: usize) -> Result<(Vec<Bot>, Vec<Identity>), tokio_postgres::Error> {
    let sql = r#"SELECT b."characterID", b."role", b."homeStationID", b."share", c."stationID", c."balance",
                        c."characterName", c."corporationID", c."securityRating", c."shipID", COALESCE(e."typeID", 0),
                        b."turf", co."allianceID"
                   FROM "bots" b JOIN "character_" c ON c."characterID" = b."characterID"
                   LEFT JOIN entity e ON e."itemID" = c."shipID"
                   LEFT JOIN "corporation" co ON co."corporationID" = c."corporationID"
                  ORDER BY b."characterID" LIMIT $1"#;
    let rows = db.query(sql, &[&(limit as i64)]).await?;
    let ids = rows
        .iter()
        .map(|r| Identity {
            name: text(r, 6).unwrap_or_default(),
            corp: int(r, 7).unwrap_or(0) as i32,
            security: float(r, 8).unwrap_or(0.0) as f32,
            ship: int(r, 9).unwrap_or(0),
            hull: int(r, 10).unwrap_or(0),
            alliance: int(r, 12).map(|a| a as i32).filter(|a| *a != 0),
        })
        .collect();
    let bots = rows
        .iter()
        .map(|r| {
            let home = int(r, 2).unwrap_or(0) as i32;
            let station = int(r, 4).map(|s| s as i32).filter(|s| atlas.station_system.contains_key(s)).unwrap_or(home);
            Bot {
                id: int(r, 0).unwrap_or(0) as i32,
                role: Role::from_i16(int(r, 1).unwrap_or(1) as i16),
                home,
                system: atlas.station_system.get(&station).copied().unwrap_or(0),
                station,
                online: false,
                share: float(r, 3).unwrap_or(0.3) as f32,
                session_end: 0,
                activity: Activity::Docked,
                route: Vec::new(),
                dock_at: 0,
                cargo: 0.0,
                isk: float(r, 5).unwrap_or(0.0),
                pod: None,
                turf: int(r, 11).unwrap_or(0) as i32,
            }
        })
        .collect();
    Ok((bots, ids))
}
