//! What losing a ship costs, done once the heartbeat has decided it happened.
//!
//! A ship goes with half of what was in and on it; the other half floats in a
//! can where it died. Its insurance pays out. The pilot ejects in a capsule.
//! A capsule lost costs the implants and, beyond what the clone keeps, skill
//! points from the pilot's biggest skill; the pilot wakes in a fresh Alpha
//! clone at its home station (see `svc::medical`). Every loss is written to
//! the kill log and mailed, in the old killmail form, to the victim and
//! whoever landed the final blow.

use std::collections::BTreeSet;

use eve_marshal::Value;
use tokio_postgres::{Client, GenericClient};
use tracing::{error, info, warn};

use crate::ballpark::{Ship, Vec3};
use crate::combat::{self, attr};
use crate::conn::Server;
use crate::items::{self, ItemTxn, NewItem};
use crate::loot::{self, Stash};
use crate::svc::medical::{GROUP_CLONE, TYPE_CLONE_ALPHA};
use crate::svc::{inflight, space};
use crate::world::Move;

const TYPE_CAPSULE: i64 = 670;
const FLAG_HANGAR: i64 = 4;
const FLAG_IMPLANT: i32 = 89;
const FLAG_SKILL: i32 = 7;
const FLAG_SKILL_IN_TRAINING: i32 = 61;
/// A ship in space sits in its solar system with no flag.
const FLAG_IN_SPACE: i64 = 0;
const ATTR_SKILL_TIME_CONSTANT: i32 = 275;
const ATTR_SKILL_POINTS: i32 = 276;
const ATTR_SKILL_LEVEL: i32 = 280;
/// market_refTypes: "Insurance".
const REF_INSURANCE: i32 = 19;
const OWNER_SYSTEM: i64 = 1;
/// Of what was in a destroyed ship, this share survives into the can.
const SURVIVING_SHARE: f64 = 0.5;

/// Who hit a ship that died, and with what.
#[derive(Debug, Clone)]
pub struct Attacker {
    /// A character, or for an NPC minus its type.
    pub who: i64,
    pub ship_type: i64,
    pub weapon_type: i64,
    pub damage: f64,
}

#[derive(Debug, Clone)]
pub struct Loss {
    pub client_id: i64,
    pub char_id: i64,
    pub ship_id: i64,
    pub ship_type: i64,
    pub system: i64,
    pub pos: Vec3,
    pub capsule: bool,
    /// Who landed the final blow, as in [`Attacker::who`].
    pub final_blow: Option<i64>,
    pub attackers: Vec<Attacker>,
}

impl Loss {
    pub fn of(ship: &Ship, system: i64, capsule: bool) -> Loss {
        let fight = ship.fight.as_ref();
        let attackers = fight
            .map(|f| {
                f.damage_log
                    .iter()
                    .map(|(who, (ship_type, weapon_type, damage))| Attacker { who: *who, ship_type: *ship_type, weapon_type: *weapon_type, damage: *damage })
                    .collect()
            })
            .unwrap_or_default();
        let final_blow = fight.and_then(|f| f.last_attacker).map(|(who, _)| who);
        Loss {
            client_id: ship.client_id,
            char_id: ship.pilot.char_id,
            ship_id: ship.id,
            ship_type: ship.pilot.type_id,
            system,
            pos: ship.pos,
            capsule,
            final_blow,
            attackers,
        }
    }
}

pub async fn handle(server: &Server, db: &mut Client, loss: Loss) {
    let report = report_text(db, &loss).await;
    let destroyed = if loss.capsule { podded(server, db, &loss).await } else { ship_lost(server, db, &loss).await };
    record(server, db, &loss, report, destroyed).await;
}

// ---- the ship -----------------------------------------------------------------------------

/// Returns what was destroyed, by type name, for the killmail.
async fn ship_lost(server: &Server, db: &mut Client, loss: &Loss) -> Vec<String> {
    let (char_id, ship_id) = (loss.char_id, loss.ship_id);
    let mut rng = combat::Rng::seeded(combat::now_ms() as u64 ^ ship_id as u64);
    let result = async {
        let mut txn = ItemTxn::begin(db).await?;
        let contents: Vec<(i64, String)> = txn
            .tx()
            .query(
                r#"SELECT e."itemID", COALESCE(t."typeName", '') FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID"
                    WHERE e."locationID" = $1 AND e."itemID" <> $2"#,
                &[&(ship_id as i32), &(char_id as i32)],
            )
            .await?
            .iter()
            .map(|r| (r.get::<_, i32>(0) as i64, r.get(1)))
            .collect();
        let mut survivors = Vec::new();
        let mut destroyed = Vec::new();
        for (id, name) in contents {
            if rng.unit() < SURVIVING_SHARE {
                survivors.push(id);
            } else {
                txn.destroy(id).await?;
                destroyed.push(name);
            }
        }
        // The survivors leave the ship before it goes.
        for id in &survivors {
            txn.move_to(*id, loss.system, 0).await?;
        }
        txn.destroy(ship_id).await?;
        let capsule = txn
            .create(NewItem { type_id: TYPE_CAPSULE, owner_id: char_id, location_id: loss.system, flag: FLAG_IN_SPACE, quantity: 1, singleton: true })
            .await?;
        txn.tx()
            .execute(r#"UPDATE "character_" SET "shipID" = $2 WHERE "characterID" = $1"#, &[&(char_id as i32), &(capsule as i32)])
            .await?;
        let events = txn.commit().await?;
        Ok::<_, tokio_postgres::Error>((capsule, events, survivors, destroyed))
    }
    .await;
    let (capsule_id, events, survivors, destroyed) = match result {
        Ok(v) => v,
        Err(e) => {
            error!("losing ship {ship_id} of {char_id}: {}", crate::logging::chain(&e));
            return Vec::new();
        }
    };
    items::notify(&server.world, loss.client_id, events);
    let stash = survivors.into_iter().map(Stash::Existing).collect();
    loot::drop_can(db, &server.world, loss.system, loss.pos, char_id, stash).await;
    pay_insurance(server, db, loss).await;

    let profile = inflight::profile(&*db, &server.dogma, char_id, capsule_id, &BTreeSet::new(), combat::now_ms()).await;
    let capsule = match profile {
        Ok(Some(p)) => {
            let mut s = Ship::new(capsule_id, loss.client_id, p.stats, p.pilot, loss.pos, Vec3::ZERO);
            s.fight = Some(p.fighter);
            s
        }
        Ok(None) => {
            error!("capsule {capsule_id} could not be read");
            return destroyed;
        }
        Err(e) => {
            error!("loading capsule {capsule_id}: {}", crate::logging::chain(&e));
            return destroyed;
        }
    };
    let mut w = server.world.lock();
    // Everyone, the pilot included, sees the capsule appear; then the
    // pilot's session moves into it, which makes it the client's ego.
    if let Some(park) = w.parks.get_mut(&loss.system) {
        park.advance(crate::destiny::tick_now());
        park.ships.insert(capsule_id, capsule);
        let entries = space::add_ball_entries(park, capsule_id);
        space::broadcast(&w, loss.system, None, entries);
    }
    w.push_move(loss.client_id, Move::ShipLost { ship_id: capsule_id });
    info!("{char_id} lost ship {ship_id}; now in capsule {capsule_id}");
    destroyed
}

async fn pay_insurance(server: &Server, db: &mut Client, loss: &Loss) {
    use crate::svc::wallet::{post, Entry, Transfer};
    let contract = match db
        .query_opt(r#"SELECT "contractID", "payout", "ownerID" FROM "insuranceContracts" WHERE "shipID" = $1"#, &[&(loss.ship_id as i32)])
        .await
    {
        Ok(Some(r)) => (r.get::<_, i32>(0), r.get::<_, f64>(1), r.get::<_, i32>(2) as i64),
        Ok(None) => return,
        Err(e) => {
            error!("reading insurance of {}: {}", loss.ship_id, crate::logging::chain(&e));
            return;
        }
    };
    let (contract_id, payout, owner) = contract;
    let arg = loss.ship_id.to_string();
    let result = async {
        let tx = db.transaction().await.map_err(Transfer::Db)?;
        tx.execute(r#"DELETE FROM "insuranceContracts" WHERE "contractID" = $1"#, &[&contract_id]).await.map_err(Transfer::Db)?;
        let entry = Entry { ref_type: REF_INSURANCE, payer: OWNER_SYSTEM, payee: owner, arg_id1: &arg, reason: "" };
        let balance = post(&tx, owner, payout, &entry).await?;
        tx.commit().await.map_err(Transfer::Db)?;
        Ok::<_, Transfer>(balance)
    }
    .await;
    match result {
        Ok(balance) => {
            info!("insurance paid {payout} ISK to {owner} for ship {}", loss.ship_id);
            let w = server.world.lock();
            let cash = Value::Tuple(vec![Value::bytes("cash"), Value::Int(owner), Value::Double(balance)]);
            w.notify(&w.clients_of(owner), "OnAccountChange", cash);
            let text = format!("Your insurance has paid out {} ISK for the loss of your ship.", crate::heartbeat::fmt_isk(payout));
            let (key, args) = crate::fight::notify(&text);
            w.notify(&w.clients_of(owner), "OnRemoteMessage", Value::Tuple(vec![Value::bytes(key), args]));
        }
        Err(Transfer::Db(e)) => error!("paying insurance for {}: {}", loss.ship_id, crate::logging::chain(&e)),
        Err(_) => warn!("insurance for {}: no wallet for {owner}", loss.ship_id),
    }
}

// ---- the capsule --------------------------------------------------------------------------

async fn podded(server: &Server, db: &mut Client, loss: &Loss) -> Vec<String> {
    let (char_id, capsule_id) = (loss.char_id, loss.ship_id);
    let result = async {
        let mut txn = ItemTxn::begin(db).await?;
        txn.item(capsule_id).await?;
        txn.destroy(capsule_id).await?;
        let implants = lose_implants(&mut txn, char_id).await?;
        let skill_loss = lose_skill_points(&txn, char_id).await?;
        // The clone's station, else where they last docked, else their corporation's.
        let chosen = crate::svc::medical::home_station(txn.tx(), char_id).await?;
        crate::svc::medical::replace_clone(txn.tx(), char_id, TYPE_CLONE_ALPHA, chosen).await?;
        let home = txn
            .tx()
            .query_opt(
                r#"SELECT s."stationID", s."solarSystemID", s."constellationID", s."regionID"
                     FROM "character_" c
                     JOIN "staStations" s ON s."stationID" = COALESCE($2, NULLIF(c."stationID", 0),
                          (SELECT co."stationID" FROM corporation co WHERE co."corporationID" = c."corporationID"))
                    WHERE c."characterID" = $1"#,
                &[&(char_id as i32), &chosen.map(|s| s as i32)],
            )
            .await?;
        let Some(home) = home else { return Ok(None) };
        let station = home.get::<_, i32>(0) as i64;
        let pod = txn
            .create(NewItem { type_id: TYPE_CAPSULE, owner_id: char_id, location_id: station, flag: FLAG_HANGAR, quantity: 1, singleton: true })
            .await?;
        txn.tx()
            .execute(
                r#"UPDATE "character_" SET "shipID" = $2, "stationID" = $3, "solarSystemID" = $4,
                          "constellationID" = $5, "regionID" = $6
                    WHERE "characterID" = $1"#,
                &[&(char_id as i32), &(pod as i32), &(station as i32), &home.get::<_, i32>(1), &home.get::<_, i32>(2), &home.get::<_, i32>(3)],
            )
            .await?;
        txn.tx()
            .execute(r#"UPDATE entity SET "locationID" = $2 WHERE "itemID" = $1"#, &[&(char_id as i32), &(station as i32)])
            .await?;
        let events = txn.commit().await?;
        let m = Move::Podded {
            station_id: station,
            solar_system_id: home.get::<_, i32>(1) as i64,
            constellation_id: home.get::<_, i32>(2) as i64,
            region_id: home.get::<_, i32>(3) as i64,
            ship_id: Some(pod),
        };
        Ok::<_, tokio_postgres::Error>(Some((m, events, implants, skill_loss)))
    }
    .await;
    match result {
        Ok(Some((m, events, implants, skill_loss))) => {
            info!("{char_id} podded; waking at {m:?}");
            items::notify(&server.world, loss.client_id, events);
            let w = server.world.lock();
            if let Some((skill, lost)) = skill_loss {
                let text = format!(
                    "Your clone could not hold everything you knew: you have lost {} skill points of {skill}. \
                     Upgrade your clone at a station's medical bay to keep more.",
                    crate::heartbeat::fmt_isk(lost)
                );
                let (key, args) = crate::fight::notify(&text);
                w.notify(&[loss.client_id], "OnRemoteMessage", Value::Tuple(vec![Value::bytes(key), args]));
            }
            w.push_move(loss.client_id, m);
            implants
        }
        Ok(None) => {
            error!("{char_id} podded with no station to wake in");
            Vec::new()
        }
        Err(e) => {
            error!("podding {char_id}: {}", crate::logging::chain(&e));
            Vec::new()
        }
    }
}

/// Destroys the implants in the pilot's head; returns their names.
async fn lose_implants(txn: &mut ItemTxn<'_>, char_id: i64) -> Result<Vec<String>, tokio_postgres::Error> {
    let rows = txn
        .tx()
        .query(
            r#"SELECT e."itemID", COALESCE(t."typeName", '') FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID"
                WHERE e."locationID" = $1 AND e."flag" = $2"#,
            &[&(char_id as i32), &FLAG_IMPLANT],
        )
        .await?;
    let mut names = Vec::new();
    for r in &rows {
        txn.destroy(r.get::<_, i32>(0) as i64).await?;
        names.push(r.get(1));
    }
    Ok(names)
}

/// Skill points a skill of `rank * 250` time constant has at `level`.
fn points_at(time_constant: f64, level: i64) -> f64 {
    if level <= 0 { 0.0 } else { time_constant * 2f64.powf(2.5 * (level - 1) as f64) }
}

/// The level `points` reach.
fn level_of(time_constant: f64, points: f64) -> i64 {
    (1..=5).rev().find(|l| points + 1e-6 >= points_at(time_constant, *l)).unwrap_or(0)
}

/// What the clone keeps; anything over comes off the biggest skill. Returns
/// (skill, points lost) if anything was.
async fn lose_skill_points(txn: &ItemTxn<'_>, char_id: i64) -> Result<Option<(String, f64)>, tokio_postgres::Error> {
    let saved: Option<f64> = txn
        .tx()
        .query_opt(
            r#"SELECT coalesce(a."valueFloat", a."valueInt"::float8)
                 FROM entity e
                 JOIN "invTypes" t ON t."typeID" = e."typeID"
                 JOIN "dgmTypeAttributes" a ON a."typeID" = e."typeID" AND a."attributeID" = $3
                WHERE e."ownerID" = $1 AND t."groupID" = $2
                ORDER BY 1 DESC LIMIT 1"#,
            &[&(char_id as i32), &GROUP_CLONE, &attr::SKILL_POINTS_SAVED],
        )
        .await?
        .and_then(|r| r.get::<_, Option<f64>>(0));
    let saved = match saved {
        Some(s) => s,
        None => txn
            .tx()
            .query_opt(
                r#"SELECT coalesce("valueFloat", "valueInt"::float8) FROM "dgmTypeAttributes" WHERE "typeID" = $1 AND "attributeID" = $2"#,
                &[&(TYPE_CLONE_ALPHA as i32), &attr::SKILL_POINTS_SAVED],
            )
            .await?
            .and_then(|r| r.get::<_, Option<f64>>(0))
            .unwrap_or(f64::MAX),
    };
    let skills = txn
        .tx()
        .query(
            r#"SELECT e."itemID", COALESCE(t."typeName", ''),
                      COALESCE((SELECT coalesce(a."valueFloat", a."valueInt"::float8) FROM entity_attributes a
                                 WHERE a."itemID" = e."itemID" AND a."attributeID" = $4), 0),
                      COALESCE((SELECT coalesce(d."valueFloat", d."valueInt"::float8) FROM "dgmTypeAttributes" d
                                 WHERE d."typeID" = e."typeID" AND d."attributeID" = $5), 250)
                 FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID"
                WHERE e."locationID" = $1 AND e."flag" IN ($2, $3)"#,
            &[&(char_id as i32), &FLAG_SKILL, &FLAG_SKILL_IN_TRAINING, &ATTR_SKILL_POINTS, &ATTR_SKILL_TIME_CONSTANT],
        )
        .await?;
    let total: f64 = skills.iter().map(|r| r.get::<_, f64>(2)).sum();
    if total <= saved {
        return Ok(None);
    }
    let Some(biggest) = skills.iter().max_by(|a, b| a.get::<_, f64>(2).total_cmp(&b.get::<_, f64>(2))) else { return Ok(None) };
    let (id, name, points, stc): (i32, String, f64, f64) = (biggest.get(0), biggest.get(1), biggest.get(2), biggest.get(3));
    let lost = (total - saved).min(points);
    let left = points - lost;
    for (a, value) in [(ATTR_SKILL_POINTS, left), (ATTR_SKILL_LEVEL, level_of(stc, left) as f64)] {
        txn.tx()
            .execute(
                r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt","valueFloat") VALUES ($1, $2, NULL, $3)
                   ON CONFLICT ("itemID","attributeID") DO UPDATE SET "valueInt" = NULL, "valueFloat" = $3"#,
                &[&id, &a, &value],
            )
            .await?;
    }
    info!("{char_id} lost {lost} skill points of {name} (clone keeps {saved}, had {total})");
    Ok(Some((name, lost)))
}

// ---- the kill report ------------------------------------------------------------------------

struct Named {
    name: String,
    corp_id: i64,
    corp: String,
    alliance_id: i64,
    alliance: String,
    security: f64,
}

async fn who(db: &impl GenericClient, char_id: i64) -> Named {
    let row = db
        .query_opt(
            r#"SELECT COALESCE(c."characterName", ''), c."corporationID", COALESCE(co."corporationName", ''),
                      COALESCE(co."allianceID", 0), COALESCE(al."allianceName", ''), c."securityRating"
                 FROM "character_" c
                 LEFT JOIN corporation co ON co."corporationID" = c."corporationID"
                 LEFT JOIN alliances al ON al."allianceID" = co."allianceID"
                WHERE c."characterID" = $1"#,
            &[&(char_id as i32)],
        )
        .await;
    match row {
        Ok(Some(r)) => Named {
            name: r.get(0),
            corp_id: r.get::<_, i32>(1) as i64,
            corp: r.get(2),
            alliance_id: r.get::<_, i32>(3) as i64,
            alliance: r.get(4),
            security: r.get(5),
        },
        _ => Named { name: format!("character {char_id}"), corp_id: 0, corp: String::new(), alliance_id: 0, alliance: String::new(), security: 0.0 },
    }
}

async fn type_name(db: &impl GenericClient, type_id: i64) -> String {
    db.query_opt(r#"SELECT COALESCE("typeName", '') FROM "invTypes" WHERE "typeID" = $1"#, &[&(type_id as i32)])
        .await
        .ok()
        .flatten()
        .map_or_else(|| format!("type {type_id}"), |r| r.get(0))
}

fn or_none(s: &str) -> &str {
    if s.is_empty() { "NONE" } else { s }
}

/// The killmail, up to its "Destroyed items" list (which only the loss knows).
async fn report_text(db: &Client, loss: &Loss) -> String {
    let victim = who(db, loss.char_id).await;
    let (system, security) = db
        .query_opt(r#"SELECT "solarSystemName", "security" FROM "mapSolarSystems" WHERE "solarSystemID" = $1"#, &[&(loss.system as i32)])
        .await
        .ok()
        .flatten()
        .map_or((String::new(), 0.0), |r| (r.get::<_, Option<String>>(0).unwrap_or_default(), r.get::<_, Option<f64>>(1).unwrap_or(0.0)));
    let mut text = format!(
        "{}\n\nVictim: {}\nAlliance: {}\nCorp: {}\nDestroyed: {}\nSystem: {}\nSecurity: {:.1}\n\nInvolved parties:\n",
        killmail_time(),
        victim.name,
        or_none(&victim.alliance),
        victim.corp,
        type_name(db, loss.ship_type).await,
        system,
        security
    );
    let mut attackers = loss.attackers.clone();
    attackers.sort_by(|a, b| b.damage.total_cmp(&a.damage));
    for a in &attackers {
        let final_blow = if Some(a.who) == loss.final_blow { " (laid the final blow)" } else { "" };
        if a.who > 0 {
            let p = who(db, a.who).await;
            text.push_str(&format!(
                "\nName: {}{final_blow}\nSecurity: {:.1}\nAlliance: {}\nCorp: {}\nShip: {}\nWeapon: {}\n",
                p.name,
                p.security,
                or_none(&p.alliance),
                p.corp,
                type_name(db, a.ship_type).await,
                type_name(db, a.weapon_type).await
            ));
        } else {
            text.push_str(&format!("\nName: {}{final_blow}\n", type_name(db, -a.who).await));
        }
    }
    text
}

fn killmail_time() -> String {
    // FILETIME to a calendar date, without a date library: days since 1601.
    let secs = crate::bulkdata::filetime_now() / 10_000_000 - 11_644_473_600;
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}.{m:02}.{d:02} {:02}:{:02}", rest / 3_600, rest % 3_600 / 60)
}

/// Howard Hinnant's days-to-civil, for 1970-based day numbers.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The kill log rows, and the killmail to the victim and the final blow.
async fn record(server: &Server, db: &mut Client, loss: &Loss, mut text: String, destroyed: Vec<String>) {
    text.push_str("\nDestroyed items:\n\n");
    for item in &destroyed {
        text.push_str(item);
        text.push('\n');
    }
    let victim = who(&*db, loss.char_id).await;
    let total: f64 = loss.attackers.iter().map(|a| a.damage).sum();
    let ship_name = type_name(&*db, loss.ship_type).await;
    let subject = format!("Kill: {} ({ship_name})", victim.name);
    let result = async {
        let tx = db.transaction().await?;
        let kill_id: i64 = tx.query_one(r#"SELECT COALESCE(MAX("killID"), 0) + 1 FROM "killLog""#, &[]).await?.get(0);
        let final_char = loss.final_blow.filter(|w| *w > 0).unwrap_or(0);
        tx.execute(
            r#"INSERT INTO "killLog" ("killID","killTime","victimCharID","victimCorpID","victimAllianceID","victimShipTypeID",
                                      "victimItemID","solarSystemID","locationID","finalBlowID","totalDamage","isPodKill")
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$8,$9,$10,$11)"#,
            &[
                &kill_id,
                &crate::bulkdata::filetime_now(),
                &(loss.char_id as i32),
                &(victim.corp_id as i32),
                &(victim.alliance_id as i32),
                &(loss.ship_type as i32),
                &(loss.ship_id as i32),
                &(loss.system as i32),
                &(final_char as i32),
                &total,
                &i16::from(loss.capsule),
            ],
        )
        .await?;
        for a in loss.attackers.iter().filter(|a| a.who > 0) {
            let p = who(&tx, a.who).await;
            tx.execute(
                r#"INSERT INTO "killLogParticipants" ("killID","ownerID","corporationID","allianceID","shipTypeID",
                                                      "weaponTypeID","damageDone","finalBlow")
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8)"#,
                &[
                    &kill_id,
                    &(a.who as i32),
                    &(p.corp_id as i32),
                    &(p.alliance_id as i32),
                    &(a.ship_type as i32),
                    &(a.weapon_type as i32),
                    &a.damage,
                    &i16::from(Some(a.who) == loss.final_blow),
                ],
            )
            .await?;
        }
        tx.commit().await?;
        Ok::<_, tokio_postgres::Error>((kill_id, final_char))
    }
    .await;
    let final_char = match result {
        Ok((kill_id, final_char)) => {
            info!("kill {kill_id}: {} lost a {ship_name}", victim.name);
            final_char
        }
        Err(e) => {
            error!("logging the loss of {}: {}", loss.ship_id, crate::logging::chain(&e));
            0
        }
    };
    let mut recipients = vec![loss.char_id];
    if final_char != 0 && final_char != loss.char_id {
        recipients.push(final_char);
    }
    for to in recipients {
        mail(server, db, to, &subject, &text).await;
    }
}

/// An EVE mail from the system to `to`.
async fn mail(server: &Server, db: &mut Client, to: i64, subject: &str, body: &str) {
    let body = body.replace('\n', "<br>");
    if let Err(e) = crate::svc::chat::deliver(&*db, &server.world, OWNER_SYSTEM, &[to], subject, &body).await {
        error!("mailing {to}: {}", crate::logging::chain(&e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_follow_the_skill_curve() {
        let stc = 250.0;
        assert_eq!(points_at(stc, 1), 250.0);
        assert!((points_at(stc, 2) - 1_414.21).abs() < 0.01);
        assert_eq!(level_of(stc, 0.0), 0);
        assert_eq!(level_of(stc, 250.0), 1);
        assert_eq!(level_of(stc, 1_414.0), 1);
        assert_eq!(level_of(stc, 256_000.0), 5);
    }

    #[test]
    fn days_become_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_390), (2025, 10, 29));
    }
}
