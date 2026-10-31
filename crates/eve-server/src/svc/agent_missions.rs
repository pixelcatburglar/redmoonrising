//! Agent missions, made up by the server: the client draws the conversation
//! and the journal, but every word and every mission comes from here
//! (agents.py __Interact, journal.py ShowAgentTab); agtMissions holds none.
//!
//! What an agent offers follows its division:
//! - courier (Distribution, Storage and the offices): carry a crate from the
//!   agent's station to one a few jumps off; it is handed over on accepting,
//!   and the mission is completed docked at the drop-off;
//! - mining (Mining, Astrosurveying): bring ore to the agent's station;
//! - trade (Manufacturing, Production, R&D): bring goods to the agent's station;
//! - encounter (Security and the like): clear the pirates at a site near the
//!   agent (`sites`), then report back from anywhere.
//!
//! The conversation is DoAction(actionID) with the actions below. Level 1
//! agents talk to anyone; higher ones want an effective standing (the best of
//! agent, corporation and faction) of 1, 3, 5 and 7, and none works for a
//! pilot one of them puts at -2 or below. Completing raises the agent's
//! standing and its corporation's, by the client's own formula
//! (standingsvc.py OnStandingsModified); quitting lowers the agent's.
//! Offers last a day, accepted missions a week; finishing within the bonus
//! time pays a bonus on top. The amounts are this server's choice.

use eve_marshal::Value;
use tokio_postgres::GenericClient;
use tracing::{error, info};

use crate::ballpark::Vec3;
use crate::combat::Rng;
use crate::items::{ItemTxn, NewItem};
use crate::services::{Ctx, Outcome};
use crate::svc::rows::{float, int, key_val, text};
use crate::svc::wallet::{self, Entry, Transfer};
use crate::world::World;

const KIND_COURIER: i16 = 1;
const KIND_MINING: i16 = 2;
const KIND_TRADE: i16 = 3;
const KIND_ENCOUNTER: i16 = 4;

const OFFERED: i16 = 0;
const ACCEPTED: i16 = 2;
const CLEARED: i16 = 3;
const COMPLETED: i16 = 4;
const DECLINED: i16 = 5;
const QUIT: i16 = 6;
const EXPIRED: i16 = 7;

/// The conversation's choices (the action IDs are this server's own).
const ASK: i64 = 1;
const ACCEPT: i64 = 2;
const DECLINE: i64 = 3;
const COMPLETE: i64 = 4;
const QUIT_MISSION: i64 = 5;

/// Agent types that hand out missions: basic, tutorial, CONCORD, storyline
/// and event agents.
const MISSION_AGENTS: [i64; 5] = [2, 3, 5, 6, 8];
const FLAG_HANGAR: i64 = 4;
const FLAG_CARGO: i64 = 5;
const TYPE_SOLAR_SYSTEM: i64 = 5;
const CATEGORY_COMMODITY: i32 = 17;

const REF_AGENT_MISSION_REWARD: i32 = 33;
const REF_AGENT_MISSION_TIME_BONUS: i32 = 34;

const SECOND: i64 = 10_000_000;
const HOUR: i64 = 3_600 * SECOND;
const OFFER_LIFETIME: i64 = 24 * HOUR;
const MISSION_LIFETIME: i64 = 7 * 24 * HOUR;

/// By level (1 to 5): the base reward, the cargo or ore volume asked for, how
/// strong an encounter's pirates are (as the security of a system with such
/// belt rats), and the standing a pilot needs.
const REWARD: [f64; 5] = [60_000.0, 220_000.0, 650_000.0, 1_600_000.0, 5_000_000.0];
const VOLUME: [f64; 5] = [5.0, 40.0, 200.0, 1_000.0, 5_000.0];
const ORE_VOLUME: [f64; 5] = [1_000.0, 3_000.0, 8_000.0, 20_000.0, 50_000.0];
const ENCOUNTER_SECURITY: [f64; 5] = [0.9, 0.6, 0.3, -0.2, -0.8];
const STANDING_NEEDED: [f64; 5] = [-10.0, 1.0, 3.0, 5.0, 7.0];
/// The ores each level asks for, commonest first.
const ORES: [&[i64]; 5] = [&[1230, 1228], &[1230, 1228, 1224], &[1224, 18, 1227], &[1227, 20, 1226], &[20, 1226, 1231, 21]];

const COURIER_NAMES: [&str; 5] = ["Urgent Delivery", "Sealed Documents", "Parcel Run", "Supply Drop", "Special Delivery"];
const MINING_NAMES: [&str; 3] = ["Ore Requisition", "Raw Materials", "Mineral Shortage"];
const TRADE_NAMES: [&str; 3] = ["Supply Order", "Procurement", "Shopping List"];
const ENCOUNTER_NAMES: [&str; 5] = ["Pirate Incursion", "Clear the Pocket", "Hunters Hunted", "Smuggler Camp", "Rogue Patrol"];

// ---- the agent and the mission -----------------------------------------------------------

struct Agent {
    id: i64,
    name: String,
    division: i64,
    corp: i64,
    faction: Option<i64>,
    station: i64,
    station_name: String,
    system: i64,
    level: usize,
    quality: i64,
    agent_type: i64,
}

async fn agent(db: &impl GenericClient, id: i64) -> Option<Agent> {
    let sql = r#"SELECT a."agentID", c."characterName", a."divisionID", a."corporationID", n."factionID", a."stationID",
                        s."stationName", s."solarSystemID", a."level", COALESCE(a."quality", 0), a."agentTypeID"
                   FROM "agtAgents" a
                   JOIN "staStations" s ON s."stationID" = a."stationID"
                   LEFT JOIN "character_" c ON c."characterID" = a."agentID"
                   LEFT JOIN "crpNPCCorporations" n ON n."corporationID" = a."corporationID"
                  WHERE a."agentID" = $1"#;
    let r = db.query_opt(sql, &[&(id as i32)]).await.ok().flatten()?;
    Some(Agent {
        id: int(&r, 0)?,
        name: text(&r, 1).unwrap_or_default(),
        division: int(&r, 2).unwrap_or(0),
        corp: int(&r, 3).unwrap_or(0),
        faction: int(&r, 4),
        station: int(&r, 5)?,
        station_name: text(&r, 6).unwrap_or_default(),
        system: int(&r, 7).unwrap_or(0),
        level: int(&r, 8).unwrap_or(1).clamp(1, 5) as usize,
        quality: int(&r, 9).unwrap_or(0),
        agent_type: int(&r, 10).unwrap_or(2),
    })
}

#[derive(Debug, Clone)]
struct Mission {
    id: i64,
    agent_id: i64,
    kind: i16,
    state: i16,
    name: String,
    type_id: i64,
    quantity: i64,
    cargo_id: Option<i64>,
    destination: i64,
    site: Option<Vec3>,
    reward: f64,
    bonus: f64,
    bonus_seconds: i64,
    accepted_at: Option<i64>,
    expires_at: i64,
}

const MISSION_SELECT: &str = r#"SELECT "missionID", "agentID", "kind", "state", "name", COALESCE("typeID", 0), "quantity", "cargoID",
                                       COALESCE("destinationID", 0), "siteX", "siteY", "siteZ", "reward", "bonus", "bonusSeconds",
                                       "acceptedAt", "expiresAt"
                                  FROM "agtCharMissions""#;

fn mission_of(r: &tokio_postgres::Row) -> Mission {
    let site = match (float(r, 9), float(r, 10), float(r, 11)) {
        (Some(x), Some(y), Some(z)) => Some(Vec3::new(x, y, z)),
        _ => None,
    };
    Mission {
        id: int(r, 0).unwrap_or(0),
        agent_id: int(r, 1).unwrap_or(0),
        kind: int(r, 2).unwrap_or(0) as i16,
        state: int(r, 3).unwrap_or(0) as i16,
        name: text(r, 4).unwrap_or_default(),
        type_id: int(r, 5).unwrap_or(0),
        quantity: int(r, 6).unwrap_or(0),
        cargo_id: int(r, 7),
        destination: int(r, 8).unwrap_or(0),
        site,
        reward: float(r, 12).unwrap_or(0.0),
        bonus: float(r, 13).unwrap_or(0.0),
        bonus_seconds: int(r, 14).unwrap_or(0),
        accepted_at: int(r, 15),
        expires_at: int(r, 16).unwrap_or(0),
    }
}

/// The character's open missions (one agent's, or all); those past their
/// time lapse on the way.
async fn open_missions(db: &impl GenericClient, char_id: i64, agent_id: Option<i64>) -> Vec<Mission> {
    let now = crate::bulkdata::filetime_now();
    let lapse = r#"UPDATE "agtCharMissions" SET "state" = $3, "closedAt" = $2
                    WHERE "characterID" = $1 AND "state" IN (0, 2, 3) AND "expiresAt" < $2"#;
    if let Err(e) = db.execute(lapse, &[&(char_id as i32), &now, &EXPIRED]).await {
        error!("lapsing missions of {char_id}: {}", crate::logging::chain(&e));
    }
    let sql = format!(r#"{MISSION_SELECT} WHERE "characterID" = $1 AND "state" IN (0, 2, 3) AND ($2::int IS NULL OR "agentID" = $2) ORDER BY "missionID""#);
    match db.query(sql.as_str(), &[&(char_id as i32), &agent_id.map(|a| a as i32)]).await {
        Ok(rows) => rows.iter().map(mission_of).collect(),
        Err(e) => {
            error!("reading missions of {char_id}: {}", crate::logging::chain(&e));
            Vec::new()
        }
    }
}

fn kind_of(division: i64) -> i16 {
    match division {
        14 | 5 => KIND_MINING,
        12 | 16 | 18 => KIND_TRADE,
        6 | 9 | 10 | 19 | 21 => KIND_ENCOUNTER,
        _ => KIND_COURIER,
    }
}

fn kind_name(kind: i16) -> &'static str {
    match kind {
        KIND_COURIER => "Courier",
        KIND_MINING => "Mining",
        KIND_TRADE => "Trade",
        _ => "Encounter",
    }
}

// ---- the conversation ----------------------------------------------------------------------

/// DoAction(actionID) -> ((agentSays, [(actionID, text)]), oob).
pub async fn conversation(ctx: &Ctx<'_>, agent_id: i64, action: Option<i64>) -> Value {
    let me = ctx.session.char_id.unwrap_or(0);
    let Some(agent) = agent(ctx.db, agent_id).await else {
        return said("I have nothing to say to you.".into(), Vec::new());
    };
    let open = open_missions(ctx.db, me, Some(agent_id)).await.into_iter().next();
    let result = match (action, open) {
        (Some(ASK), None) => ask(ctx, &agent).await,
        (Some(ACCEPT), Some(m)) if m.state == OFFERED => accept(ctx, &agent, m).await,
        (Some(DECLINE), Some(m)) if m.state == OFFERED => close(ctx, &agent, &m, DECLINED, "declined").await.map(|_| {
            said("Very well. Perhaps another time.".into(), Vec::new())
        }),
        (Some(COMPLETE), Some(m)) if m.state != OFFERED => complete(ctx, &agent, m).await,
        (Some(QUIT_MISSION), Some(m)) if m.state != OFFERED => quit(ctx, &agent, &m).await,
        (_, Some(m)) => Ok(status(ctx, &agent, &m).await),
        (_, None) => Ok(said(
            format!("Hello, {}. What can I do for you?", html(&ctx.session.char_name)),
            vec![(ASK, "Do you have any work for me?")],
        )),
    };
    result.unwrap_or_else(|why| said(why, Vec::new()))
}

fn said(says: String, choices: Vec<(i64, &str)>) -> Value {
    let choices = choices.into_iter().map(|(id, t)| Value::Tuple(vec![Value::Int(id), Value::text(t)])).collect();
    Value::Tuple(vec![Value::Tuple(vec![Value::text(says), Value::List(choices)]), Value::Dict(Vec::new())])
}

fn html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

async fn ask(ctx: &Ctx<'_>, agent: &Agent) -> Result<Value, String> {
    if !MISSION_AGENTS.contains(&agent.agent_type) {
        return Err("I don't hand out missions. Perhaps one of my colleagues can help you.".into());
    }
    let me = ctx.session.char_id.unwrap_or(0);
    let standings = standings_toward(ctx.db, me, agent).await;
    if standings.iter().any(|s| *s <= -2.0) {
        return Err("I won't work with you. Mend your relations with us first.".into());
    }
    let best = standings.iter().copied().fold(0.0, f64::max);
    let needed = STANDING_NEEDED[agent.level - 1];
    if best < needed {
        return Err(format!(
            "I'm afraid I can't trust you with my work yet. You'll need an effective standing of {needed:.1} with me, my corporation or its faction; yours is {best:.2}."
        ));
    }
    let mission = offer(ctx, agent).await.ok_or_else(|| "I have nothing for you right now. Check back with me later.".to_string())?;
    notify_change(ctx.world, me, "offered", agent.id);
    Ok(briefing(ctx, agent, &mission).await)
}

/// The offer, with what it pays and what the pilot may answer.
async fn briefing(ctx: &Ctx<'_>, agent: &Agent, m: &Mission) -> Value {
    let mut says = format!("<b>{}</b><br><br>{}", html(&m.name), objective(ctx.db, agent, m).await);
    says.push_str(&format!("<br><br>{}", rewards(m)));
    said(says, vec![(ACCEPT, "Accept"), (DECLINE, "Decline")])
}

fn rewards(m: &Mission) -> String {
    format!(
        "Reward: {} ISK<br>Bonus: {} ISK if completed within {}.",
        isk(m.reward),
        isk(m.bonus),
        interval(m.bonus_seconds)
    )
}

async fn status(ctx: &Ctx<'_>, agent: &Agent, m: &Mission) -> Value {
    if m.state == OFFERED {
        return briefing(ctx, agent, m).await;
    }
    let mut says = format!("<b>{}</b><br><br>{}", html(&m.name), objective(ctx.db, agent, m).await);
    if m.state == CLEARED {
        says.push_str("<br><br>I hear the pirates are gone. Good work.");
    }
    said(says, vec![(COMPLETE, "I have completed the mission."), (QUIT_MISSION, "I want to quit this mission.")])
}

/// What the pilot has to do, in the agent's words.
async fn objective(db: &impl GenericClient, agent: &Agent, m: &Mission) -> String {
    let what = html(&type_name(db, m.type_id).await);
    match m.kind {
        KIND_COURIER => {
            let (station, system) = station_names(db, m.destination).await;
            format!(
                "I need {} unit(s) of {what} taken from here, {}, to {} in {}. You'll find the cargo in your hangar here once you accept.",
                m.quantity,
                html(&agent.station_name),
                html(&station),
                html(&system)
            )
        }
        KIND_MINING => format!("Bring me {} unit(s) of {what}, here at {}.", m.quantity, html(&agent.station_name)),
        KIND_TRADE => format!("I'm short of {} unit(s) of {what}. Get them to me here at {}.", m.quantity, html(&agent.station_name)),
        _ => format!(
            "A band of pirates has set up in {}. Clear them out; the location is in your journal. Report back to me when it's done.",
            html(&system_name(db, m.destination).await)
        ),
    }
}

// ---- making an offer -----------------------------------------------------------------------

async fn offer(ctx: &Ctx<'_>, agent: &Agent) -> Option<Mission> {
    let me = ctx.session.char_id.unwrap_or(0);
    let now = crate::bulkdata::filetime_now();
    let mut rng = Rng::seeded(now as u64 ^ (me as u64) << 16 ^ agent.id as u64);
    let level = agent.level;
    let quality = 1.0 + agent.quality as f64 / 50.0;
    let base = REWARD[level - 1] * quality;
    let kind = kind_of(agent.division);
    let pick = |rng: &mut Rng, names: &[&'static str]| names[rng.below(names.len())];
    let (name, type_id, quantity, destination, site, reward, bonus, bonus_hours) = match kind {
        KIND_COURIER => {
            let reach = (level as i64 + 1).min(6);
            let systems: Vec<(i64, i64)> = crate::gates::within(ctx.db, agent.system, reach).await;
            let ids: Vec<i32> = systems.iter().map(|(s, _)| *s as i32).collect();
            let sql = r#"SELECT "stationID", "solarSystemID" FROM "staStations" WHERE "solarSystemID" = ANY($1) AND "stationID" <> $2"#;
            let stations: Vec<(i64, i64)> = ctx
                .query(sql, &[&ids, &(agent.station as i32)])
                .await
                .iter()
                .filter_map(|r| Some((int(r, 0)?, int(r, 1)?)))
                .collect();
            let &(station, system) = stations.get(rng.below(stations.len().max(1)))?;
            let jumps = systems.iter().find(|(s, _)| *s == system).map_or(0, |(_, n)| *n);
            let (cargo, volume, _) = commodity(ctx.db, &mut rng, 0.1, 10.0).await?;
            let quantity = (VOLUME[level - 1] * rng.range(0.5, 1.0) / volume).ceil().max(1.0) as i64;
            let reward = base * (0.6 + 0.15 * jumps as f64);
            (pick(&mut rng, &COURIER_NAMES), cargo, quantity, station, None, reward, reward * 0.5, 1.0 + jumps as f64 * 0.25)
        }
        KIND_MINING => {
            let ores = ORES[level - 1];
            let ore = ores[rng.below(ores.len()).min(rng.below(ores.len()))];
            let volume = type_volume(ctx.db, ore).await.max(0.01);
            let quantity = (ORE_VOLUME[level - 1] * rng.range(0.6, 1.0) / volume).round().max(1.0) as i64;
            (pick(&mut rng, &MINING_NAMES), ore, quantity, agent.station, None, base, base * 0.3, 4.0)
        }
        KIND_TRADE => {
            let (goods, _, price) = commodity(ctx.db, &mut rng, 0.01, 50.0).await?;
            let budget = base * 0.5;
            let quantity = (budget / price.max(1.0)).round().clamp(1.0, 100_000.0) as i64;
            let reward = base * 0.5 + price * quantity as f64 * 1.15;
            (pick(&mut rng, &TRADE_NAMES), goods, quantity, agent.station, None, reward, base * 0.2, 4.0)
        }
        _ => {
            let systems = crate::gates::within(ctx.db, agent.system, level as i64 - 1).await;
            let &(system, _) = systems.get(rng.below(systems.len().max(1)))?;
            let at = site_position(ctx.db, system, &mut rng).await;
            let reward = base * 1.3;
            (pick(&mut rng, &ENCOUNTER_NAMES), 0, 0, system, Some(at), reward, reward * 0.4, 2.0)
        }
    };
    let sql = r#"INSERT INTO "agtCharMissions" ("characterID", "agentID", "kind", "state", "name", "typeID", "quantity",
                                                "destinationID", "siteX", "siteY", "siteZ", "reward", "bonus", "bonusSeconds",
                                                "offeredAt", "expiresAt")
                 VALUES ($1, $2, $3, 0, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
                 RETURNING "missionID""#;
    let i = |v: i64| v as i32;
    let (bonus_seconds, expires) = ((bonus_hours * 3_600.0) as i32, now + OFFER_LIFETIME);
    let (x, y, z) = (site.map(|s: Vec3| s.x), site.map(|s: Vec3| s.y), site.map(|s: Vec3| s.z));
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 15] = [
        &i(me), &i(agent.id), &kind, &name, &(type_id != 0).then_some(i(type_id)), &i(quantity), &i(destination),
        &x, &y, &z, &reward.round(), &bonus.round(), &bonus_seconds, &now, &expires,
    ];
    let id = ctx.query_one(sql, &params).await.and_then(|r| int(&r, 0))?;
    info!("agent {} offered {me} mission {id}: {} ({name})", agent.id, kind_name(kind));
    Some(Mission {
        id,
        agent_id: agent.id,
        kind,
        state: OFFERED,
        name: name.into(),
        type_id,
        quantity,
        cargo_id: None,
        destination,
        site,
        reward: reward.round(),
        bonus: bonus.round(),
        bonus_seconds: bonus_seconds as i64,
        accepted_at: None,
        expires_at: expires,
    })
}

/// A commodity of one unit's volume within bounds: (typeID, volume, base price).
async fn commodity(db: &impl GenericClient, rng: &mut Rng, min_volume: f64, max_volume: f64) -> Option<(i64, f64, f64)> {
    let sql = r#"SELECT t."typeID", t."volume", t."basePrice" FROM "invTypes" t JOIN "invGroups" g ON g."groupID" = t."groupID"
                  WHERE g."categoryID" = $1 AND t."marketGroupID" IS NOT NULL AND t."basePrice" > 0
                    AND t."volume" BETWEEN $2 AND $3
                  ORDER BY t."typeID""#;
    let rows = db.query(sql, &[&CATEGORY_COMMODITY, &min_volume, &max_volume]).await.ok()?;
    let r = rows.get(rng.below(rows.len().max(1)))?;
    Some((int(r, 0)?, float(r, 1).unwrap_or(1.0), float(r, 2).unwrap_or(1.0)))
}

/// Somewhere quiet in a system: a few thousand km off one of its planets,
/// or an AU from the sun if it has none.
async fn site_position(db: &impl GenericClient, system: i64, rng: &mut Rng) -> Vec3 {
    let sql = r#"SELECT "x", "y", "z", COALESCE("radius", 0) FROM "mapDenormalize" WHERE "solarSystemID" = $1 AND "groupID" = 7"#;
    let planets: Vec<(Vec3, f64)> = db
        .query(sql, &[&(system as i32)])
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| Some((Vec3::new(float(r, 0)?, float(r, 1)?, float(r, 2)?), float(r, 3).unwrap_or(0.0))))
        .collect();
    let (centre, radius) = planets.get(rng.below(planets.len().max(1))).copied().unwrap_or((Vec3::ZERO, 1.5e11));
    let out = rng.in_sphere(1.0);
    let dir = if out.len() > 1e-6 { out.unit() } else { Vec3::new(1.0, 0.0, 0.0) };
    centre + dir * (radius + rng.range(3.0e6, 9.0e6))
}

// ---- accepting, completing, quitting -------------------------------------------------------

async fn accept(ctx: &Ctx<'_>, agent: &Agent, m: Mission) -> Result<Value, String> {
    let me = ctx.session.char_id.unwrap_or(0);
    let now = crate::bulkdata::filetime_now();
    let mut cargo = None;
    if m.kind == KIND_COURIER {
        if ctx.session.station_id != Some(agent.station) {
            return Err(format!("Come and see me at {} to pick up the cargo.", html(&agent.station_name)));
        }
        let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| e.to_string())?;
        let mut txn = ItemTxn::begin(&mut client).await.map_err(|e| e.to_string())?;
        let crate_ = NewItem { type_id: m.type_id, owner_id: me, location_id: agent.station, flag: FLAG_HANGAR, quantity: m.quantity, singleton: false };
        cargo = Some(txn.create(crate_).await.map_err(|e| e.to_string())?);
        let events = txn.commit().await.map_err(|e| e.to_string())?;
        crate::items::notify(ctx.world, ctx.client_id, events);
    }
    let sql = r#"UPDATE "agtCharMissions" SET "state" = $2, "acceptedAt" = $3, "expiresAt" = $4, "cargoID" = $5
                  WHERE "missionID" = $1 AND "state" = 0"#;
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 5] =
        [&(m.id as i32), &ACCEPTED, &now, &(now + MISSION_LIFETIME), &cargo.map(|c| c as i32)];
    ctx.execute(sql, &params).await;
    info!("{me} accepted mission {} from agent {}", m.id, agent.id);
    notify_change(ctx.world, me, "accepted", agent.id);
    let accepted = Mission { state: ACCEPTED, accepted_at: Some(now), cargo_id: cargo, ..m };
    let mut says = format!("Excellent. {}", objective(ctx.db, agent, &accepted).await);
    says.push_str("<br><br>Come back to me when you're done.");
    Ok(said(says, Vec::new()))
}

async fn complete(ctx: &Ctx<'_>, agent: &Agent, m: Mission) -> Result<Value, String> {
    let me = ctx.session.char_id.unwrap_or(0);
    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| e.to_string())?;
    let mut txn = ItemTxn::begin(&mut client).await.map_err(|e| e.to_string())?;
    match m.kind {
        KIND_COURIER => {
            if ctx.session.station_id != Some(m.destination) {
                let (station, _) = station_names(ctx.db, m.destination).await;
                return Err(format!("The cargo has to reach {} first.", html(&station)));
            }
            let item = match m.cargo_id {
                Some(id) => txn.item(id).await.map_err(|e| e.to_string())?,
                None => None,
            };
            let delivered = match &item {
                Some(i) if i.location_id == m.destination && i.owner_id == me => true,
                Some(i) if Some(i.location_id) == ctx.session.ship_id => true,
                _ => false,
            };
            if !delivered || item.as_ref().is_some_and(|i| i.quantity < m.quantity) {
                return Err("I don't see the cargo there. Bring all of it to the drop-off and talk to me again.".into());
            }
            txn.destroy(m.cargo_id.unwrap_or(0)).await.map_err(|e| e.to_string())?;
        }
        KIND_MINING | KIND_TRADE => {
            if ctx.session.station_id != Some(agent.station) {
                return Err(format!("Bring them to me at {}.", html(&agent.station_name)));
            }
            let mut left = m.quantity;
            let sources = [(agent.station, FLAG_HANGAR), (ctx.session.ship_id.unwrap_or(0), FLAG_CARGO)];
            let mut stacks = Vec::new();
            for (location, flag) in sources {
                let sql = r#"SELECT "itemID", "quantity" FROM entity
                              WHERE "locationID" = $1 AND "ownerID" = $2 AND "flag" = $3 AND "typeID" = $4 AND "singleton" = 0
                              ORDER BY "itemID" FOR UPDATE"#;
                let rows = txn
                    .tx()
                    .query(sql, &[&(location as i32), &(me as i32), &(flag as i32), &(m.type_id as i32)])
                    .await
                    .map_err(|e| e.to_string())?;
                stacks.extend(rows.iter().map(|r| (int(r, 0).unwrap_or(0), int(r, 1).unwrap_or(0))));
            }
            if stacks.iter().map(|(_, q)| q).sum::<i64>() < m.quantity {
                return Err(format!("You don't have {} unit(s) of {} here yet.", m.quantity, html(&type_name(ctx.db, m.type_id).await)));
            }
            for (id, q) in stacks {
                if left == 0 {
                    break;
                }
                let take = q.min(left);
                txn.set_quantity(id, q - take).await.map_err(|e| e.to_string())?;
                left -= take;
            }
        }
        _ if m.state != CLEARED => return Err("Those pirates are still out there.".into()),
        _ => {}
    }

    let now = crate::bulkdata::filetime_now();
    let in_time = m.accepted_at.is_some_and(|a| now - a <= m.bonus_seconds * SECOND);
    let reason = format!("Mission: {}", m.name);
    let mut balance = 0.0;
    for (amount, ref_type) in [(m.reward, REF_AGENT_MISSION_REWARD), (if in_time { m.bonus } else { 0.0 }, REF_AGENT_MISSION_TIME_BONUS)] {
        if amount <= 0.0 {
            continue;
        }
        let entry = Entry { ref_type, payer: agent.id, payee: me, arg_id1: "", reason: &reason };
        balance = match wallet::post(txn.tx(), me, amount, &entry).await {
            Ok(b) => b,
            Err(Transfer::Db(e)) => return Err(format!("The reward could not be paid: {e}")),
            Err(_) => return Err("Your wallet could not be found.".into()),
        };
    }
    let done = r#"UPDATE "agtCharMissions" SET "state" = $2, "closedAt" = $3 WHERE "missionID" = $1"#;
    txn.tx().execute(done, &[&(m.id as i32), &COMPLETED, &now]).await.map_err(|e| e.to_string())?;
    let events = txn.commit().await.map_err(|e| e.to_string())?;
    crate::items::notify(ctx.world, ctx.client_id, events);
    let gain = 0.01 * (1.0 + agent.level as f64);
    modify_standing(ctx.db, ctx.world, me, agent.id, gain).await;
    modify_standing(ctx.db, ctx.world, me, agent.corp, gain / 2.0).await;
    close_site(ctx.world, m.id);
    {
        let w = ctx.world.lock();
        let cash = Value::Tuple(vec![Value::bytes("cash"), Value::Int(me), Value::Double(balance)]);
        w.notify(&w.clients_of(me), "OnAccountChange", cash);
    }
    notify_change(ctx.world, me, "completed", agent.id);
    info!("{me} completed mission {} for agent {}{}", m.id, agent.id, if in_time { " in time for the bonus" } else { "" });
    let paid = m.reward + if in_time { m.bonus } else { 0.0 };
    Ok(said(format!("Thank you, {}. I've transferred {} ISK to your account.", html(&ctx.session.char_name), isk(paid)), Vec::new()))
}

async fn quit(ctx: &Ctx<'_>, agent: &Agent, m: &Mission) -> Result<Value, String> {
    close(ctx, agent, m, QUIT, "quit").await?;
    let me = ctx.session.char_id.unwrap_or(0);
    modify_standing(ctx.db, ctx.world, me, agent.id, -0.02 * agent.level as f64).await;
    close_site(ctx.world, m.id);
    Ok(said("That's disappointing. I'll remember this.".into(), Vec::new()))
}

async fn close(ctx: &Ctx<'_>, agent: &Agent, m: &Mission, state: i16, what: &'static str) -> Result<(), String> {
    let me = ctx.session.char_id.unwrap_or(0);
    let sql = r#"UPDATE "agtCharMissions" SET "state" = $2, "closedAt" = $3 WHERE "missionID" = $1"#;
    ctx.execute(sql, &[&(m.id as i32), &state, &crate::bulkdata::filetime_now()]).await;
    info!("{me} {what} mission {} of agent {}", m.id, agent.id);
    notify_change(ctx.world, me, what, agent.id);
    Ok(())
}

/// An encounter's site is shut when its mission is over.
fn close_site(world: &World, mission_id: i64) {
    let mut w = world.lock();
    let systems: Vec<i64> = w.parks.iter().filter(|(_, p)| p.sites.values().any(|s| s.mission_id == mission_id)).map(|(id, _)| *id).collect();
    for system in systems {
        let entries = w.parks.get_mut(&system).map(|p| crate::sites::close(p, mission_id)).unwrap_or_default();
        crate::svc::space::broadcast(&w, system, None, entries);
    }
}

/// Heartbeat job: an encounter's last pirate is dead.
pub async fn site_cleared(world: &World, db: &tokio_postgres::Client, mission_id: i64, char_id: i64) {
    let sql = r#"UPDATE "agtCharMissions" SET "state" = $3 WHERE "missionID" = $1 AND "characterID" = $2 AND "state" = $4 RETURNING "agentID""#;
    match db.query_opt(sql, &[&(mission_id as i32), &(char_id as i32), &CLEARED, &ACCEPTED]).await {
        Ok(Some(r)) => {
            let agent = int(&r, 0).unwrap_or(0);
            info!("mission {mission_id} of {char_id}: the site is cleared");
            notify_change(world, char_id, "objective", agent);
            let w = world.lock();
            let note = Value::Dict(vec![(Value::bytes("info"), Value::text("The pirates are gone. Report back to your agent."))]);
            w.notify(&w.clients_of(char_id), "OnRemoteMessage", Value::Tuple(vec![Value::bytes("CustomInfo"), note]));
        }
        Ok(None) => {}
        Err(e) => error!("recording the clearing of mission {mission_id}: {}", crate::logging::chain(&e)),
    }
}

// ---- standings -------------------------------------------------------------------------------

/// The pilot's standing from the agent, its corporation and its faction.
async fn standings_toward(db: &impl GenericClient, me: i64, agent: &Agent) -> Vec<f64> {
    let ids: Vec<i32> = [Some(agent.id), Some(agent.corp), agent.faction].into_iter().flatten().map(|i| i as i32).collect();
    let sql = r#"SELECT "standing" FROM "chrNPCStandings" WHERE "characterID" = $1 AND "fromID" = ANY($2)"#;
    let mut found: Vec<f64> = db.query(sql, &[&(me as i32), &ids]).await.unwrap_or_default().iter().filter_map(|r| float(r, 0)).collect();
    found.resize(ids.len(), 0.0);
    found
}

/// A standing moved by `m` the way the client moves it (standingsvc.py
/// OnStandingsModified): a gain closes that share of the gap to 10, a loss
/// that share of the way down to -10.
pub fn modified(standing: f64, m: f64) -> f64 {
    let s = standing / 10.0;
    let new = if m > 0.0 { 1.0 - (1.0 - s) * (1.0 - m) } else { s + (1.0 + s) * m };
    (new * 10.0).clamp(-10.0, 10.0)
}

async fn modify_standing(db: &impl GenericClient, world: &World, me: i64, from: i64, m: f64) {
    let sql = r#"SELECT "standing" FROM "chrNPCStandings" WHERE "characterID" = $1 AND "fromID" = $2"#;
    let old = db.query_opt(sql, &[&(me as i32), &(from as i32)]).await.ok().flatten().and_then(|r| float(&r, 0)).unwrap_or(0.0);
    let new = modified(old, m);
    let save = r#"INSERT INTO "chrNPCStandings" ("characterID", "fromID", "standing") VALUES ($1, $2, $3)
                  ON CONFLICT ("characterID", "fromID") DO UPDATE SET "standing" = $3"#;
    if let Err(e) = db.execute(save, &[&(me as i32), &(from as i32), &new]).await {
        error!("saving the standing of {from} toward {me}: {}", crate::logging::chain(&e));
        return;
    }
    let w = world.lock();
    w.notify(&w.clients_of(me), "OnStandingSet", Value::Tuple(vec![Value::Int(from), Value::Int(me), Value::Double(new)]));
}

fn notify_change(world: &World, char_id: i64, what: &str, agent_id: i64) {
    let w = world.lock();
    w.notify(&w.clients_of(char_id), "OnAgentMissionChange", Value::Tuple(vec![Value::text(what), Value::Int(agent_id)]));
}

// ---- the journal -----------------------------------------------------------------------------

/// GetMyJournalDetails() -> (missions, offers, research): each mission as
/// (missionState, important, missionType, missionName, agentID,
/// expirationTime, bookmarks) (journal.py:241).
pub async fn journal(ctx: &Ctx<'_>, agent_id: Option<i64>) -> Value {
    let me = ctx.session.char_id.unwrap_or(0);
    let mut missions = Vec::new();
    for m in open_missions(ctx.db, me, agent_id).await {
        let state = if m.state == OFFERED { 0 } else { 2 };
        let bookmarks = bookmarks(ctx.db, &m).await;
        missions.push(Value::Tuple(vec![
            Value::Int(state),
            Value::Int(0),
            Value::text(kind_name(m.kind)),
            Value::text(&m.name),
            Value::Int(m.agent_id),
            Value::Long(m.expires_at),
            Value::List(bookmarks),
        ]));
    }
    Value::Tuple(vec![Value::List(missions), Value::List(Vec::new()), Value::List(Vec::new())])
}

/// The places a mission sends the pilot, as the address book merges them
/// into its own (addressbook.py GetMapBookmarks).
async fn bookmarks(db: &impl GenericClient, m: &Mission) -> Vec<Value> {
    let bookmark = |location_type: &str, hint: String, item: i64, type_id: i64, system: i64, at: Vec3| {
        key_val(vec![
            ("agentID", Value::Int(m.agent_id)),
            ("locationType", Value::text(location_type)),
            ("locationNumber", Value::Int(m.id)),
            ("hint", Value::text(hint)),
            ("itemID", Value::Int(item)),
            ("typeID", Value::Int(type_id)),
            ("locationID", Value::Int(system)),
            ("x", Value::Double(at.x)),
            ("y", Value::Double(at.y)),
            ("z", Value::Double(at.z)),
        ])
    };
    match (m.kind, m.site) {
        (KIND_ENCOUNTER, Some(at)) => {
            let name = system_name(db, m.destination).await;
            vec![bookmark("dungeon", format!("Objective ({name})"), m.destination, TYPE_SOLAR_SYSTEM, m.destination, at)]
        }
        (KIND_COURIER, _) => {
            let sql = r#"SELECT "stationTypeID", "solarSystemID", "x", "y", "z", "stationName" FROM "staStations" WHERE "stationID" = $1"#;
            let Some(r) = db.query_opt(sql, &[&(m.destination as i32)]).await.ok().flatten() else { return Vec::new() };
            let at = Vec3::new(float(&r, 2).unwrap_or(0.0), float(&r, 3).unwrap_or(0.0), float(&r, 4).unwrap_or(0.0));
            let hint = format!("Drop-off ({})", text(&r, 5).unwrap_or_default());
            vec![bookmark("dropoff", hint, m.destination, int(&r, 0).unwrap_or(0), int(&r, 1).unwrap_or(0), at)]
        }
        _ => Vec::new(),
    }
}

/// GetMissionJournalInfo(): the mission's page, or None without one.
pub async fn mission_page(ctx: &Ctx<'_>, agent_id: i64) -> Value {
    let me = ctx.session.char_id.unwrap_or(0);
    let Some(agent) = agent(ctx.db, agent_id).await else { return Value::None };
    let Some(m) = open_missions(ctx.db, me, Some(agent_id)).await.into_iter().next() else { return Value::None };
    let state = match m.state {
        OFFERED => "Offered",
        CLEARED => "Objective complete; report to the agent",
        _ => "Accepted",
    };
    let page = format!(
        "<HTML><BODY><H3>{}</H3><TABLE><TR><TD>Agent:</TD><TD>{}</TD></TR><TR><TD>Type:</TD><TD>{}</TD></TR><TR><TD>State:</TD><TD>{}</TD></TR></TABLE><BR>{}<BR><BR>{}</BODY></HTML>",
        html(&m.name),
        html(&agent.name),
        kind_name(m.kind),
        state,
        objective(ctx.db, &agent, &m).await,
        rewards(&m)
    );
    Value::text(page)
}

// ---- going there -----------------------------------------------------------------------------

/// WarpToLocation(locationType, locationNumber, warpRange, gang), and with
/// `approach` GotoLocation(locationType, locationNumber): to a mission's
/// place in the pilot's own system. An encounter's site opens as they set out.
pub async fn go_to(ctx: &Ctx<'_>, number: i64, range: f64, approach: bool) -> Outcome {
    let me = ctx.session.char_id.unwrap_or(0);
    let Some(system) = ctx.session.solar_system_id else { return Outcome::custom("You have to be in space for that.") };
    let sql = format!(r#"{MISSION_SELECT} WHERE "missionID" = $1 AND "characterID" = $2 AND "state" IN (2, 3)"#);
    let Some(m) = ctx.query_one(sql.as_str(), &[&(number as i32), &(me as i32)]).await.map(|r| mission_of(&r)) else {
        return Outcome::custom("That mission is not yours, or is over.");
    };
    let (at, radius) = match (m.kind, m.site) {
        (KIND_ENCOUNTER, Some(at)) => {
            if m.destination != system {
                return Outcome::custom("That location is in another solar system. Set your destination there first.");
            }
            if m.state == ACCEPTED {
                open_site(ctx, &m, system, at).await;
            }
            (at, 0.0)
        }
        _ => {
            let sql = r#"SELECT "x", "y", "z", COALESCE("radius", 0), "solarSystemID" FROM "mapDenormalize" WHERE "itemID" = $1"#;
            let Some(r) = ctx.query_one(sql, &[&(m.destination as i32)]).await else { return Outcome::custom("That station is gone.") };
            if int(&r, 4) != Some(system) {
                return Outcome::custom("That location is in another solar system. Set your destination there first.");
            }
            (Vec3::new(float(&r, 0).unwrap_or(0.0), float(&r, 1).unwrap_or(0.0), float(&r, 2).unwrap_or(0.0)), float(&r, 3).unwrap_or(0.0))
        }
    };
    if approach {
        crate::svc::space::approach_point(ctx, at, "GotoLocation")
    } else {
        crate::svc::space::warp_to_point(ctx, at, radius, range, "WarpToLocation")
    }
}

async fn open_site(ctx: &Ctx<'_>, m: &Mission, system: i64, at: Vec3) {
    let level = agent(ctx.db, m.agent_id).await.map_or(1, |a| a.level);
    let mut w = ctx.world.lock();
    let Some(park) = w.parks.get_mut(&system) else { return };
    if park.sites.values().any(|s| s.mission_id == m.id) {
        return;
    }
    let site = crate::sites::Site {
        id: crate::npc::next_id(),
        mission_id: m.id,
        char_id: ctx.session.char_id.unwrap_or(0),
        name: m.name.clone(),
        pos: at,
        rats: Vec::new(),
    };
    let entries = crate::sites::open(park, site, ENCOUNTER_SECURITY[level - 1], crate::combat::now_ms());
    crate::svc::space::broadcast(&w, system, None, entries);
}

// ---- names and amounts -----------------------------------------------------------------------

async fn type_name(db: &impl GenericClient, type_id: i64) -> String {
    let sql = r#"SELECT "typeName" FROM "invTypes" WHERE "typeID" = $1"#;
    db.query_opt(sql, &[&(type_id as i32)]).await.ok().flatten().and_then(|r| text(&r, 0)).unwrap_or_default()
}

async fn type_volume(db: &impl GenericClient, type_id: i64) -> f64 {
    let sql = r#"SELECT "volume" FROM "invTypes" WHERE "typeID" = $1"#;
    db.query_opt(sql, &[&(type_id as i32)]).await.ok().flatten().and_then(|r| float(&r, 0)).unwrap_or(1.0)
}

async fn system_name(db: &impl GenericClient, system: i64) -> String {
    let sql = r#"SELECT "solarSystemName" FROM "mapSolarSystems" WHERE "solarSystemID" = $1"#;
    db.query_opt(sql, &[&(system as i32)]).await.ok().flatten().and_then(|r| text(&r, 0)).unwrap_or_default()
}

/// A station's name and its system's.
async fn station_names(db: &impl GenericClient, station: i64) -> (String, String) {
    let sql = r#"SELECT s."stationName", m."solarSystemName" FROM "staStations" s
                   JOIN "mapSolarSystems" m ON m."solarSystemID" = s."solarSystemID" WHERE s."stationID" = $1"#;
    match db.query_opt(sql, &[&(station as i32)]).await.ok().flatten() {
        Some(r) => (text(&r, 0).unwrap_or_default(), text(&r, 1).unwrap_or_default()),
        None => (String::new(), String::new()),
    }
}

fn isk(amount: f64) -> String {
    let whole = amount.round() as i64;
    let digits = whole.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if whole < 0 { format!("-{out}") } else { out }
}

fn interval(seconds: i64) -> String {
    let (h, m) = (seconds / 3_600, seconds % 3_600 / 60);
    match (h, m) {
        (0, m) => format!("{m} minutes"),
        (h, 0) => format!("{h} hour(s)"),
        (h, m) => format!("{h} hour(s) {m} minutes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standings_move_as_the_client_moves_them() {
        assert!((modified(0.0, 0.1) - 1.0).abs() < 1e-9);
        assert!((modified(5.0, 0.1) - 5.5).abs() < 1e-9);
        assert!((modified(0.0, -0.1) - -1.0).abs() < 1e-9);
        assert_eq!(modified(10.0, 0.5), 10.0);
    }

    #[test]
    fn divisions_choose_the_kind_of_work() {
        assert_eq!(kind_of(7), KIND_COURIER);
        assert_eq!(kind_of(14), KIND_MINING);
        assert_eq!(kind_of(12), KIND_TRADE);
        assert_eq!(kind_of(19), KIND_ENCOUNTER);
    }

    #[test]
    fn amounts_read_as_the_client_writes_them() {
        assert_eq!(isk(1_234_567.4), "1,234,567");
        assert_eq!(isk(60_000.0), "60,000");
        assert_eq!(isk(999.0), "999");
        assert_eq!(interval(5_400), "1 hour(s) 30 minutes");
        assert_eq!(interval(7_200), "2 hour(s)");
    }
}
