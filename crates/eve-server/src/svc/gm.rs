//! Game master tools: `slash` (the chat window's /commands, lscchannel.py:541,
//! and the GM menu's "TR me here!"), and the dungeon tools' services, which
//! have no dungeons to work on.
//!
//! The client sends slash commands only from a session with ROLE_GML or
//! ROLE_LEGIONEER (lscchannel.py ROLE_SLASH), and the server holds to the
//! same. The commands:
//! - /create <type> [quantity]: into the hangar, or the hold in space;
//! - /giveisk <amount>;
//! - /giveskill <skill> [level]: trained, straight into the character;
//! - /setstanding <ownerID> <standing>: an NPC's standing toward you;
//! - /spawn: a group of the system's rats, around your ship;
//! - /tr me <station | system | celestial | character>;
//! - /help.
//! A type or a skill is its ID or its name.

use eve_marshal::Value;
use tokio_postgres::GenericClient;
use tracing::info;

use crate::ballpark::Vec3;
use crate::items::{ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{float, int, text};
use crate::svc::wallet::{self, Entry};
use crate::world::Move;

const ROLE_GML: i64 = 8;
const ROLE_LEGIONEER: i64 = 262_144;
const FLAG_HANGAR: i64 = 4;
const FLAG_CARGO: i64 = 5;
const FLAG_SKILL: i64 = 7;
const CATEGORY_SKILL: i32 = 16;
const ATTR_SKILL_TIME_CONSTANT: i32 = 275;
const ATTR_SKILL_POINTS: i32 = 276;
const ATTR_SKILL_LEVEL: i32 = 280;
/// refAgentMiscellaneous, for money a GM conjures.
const REF_GM: i32 = 25;
const OWNER_SYSTEM: i64 = 1;

const HELP: &str = "/create <type> [qty], /giveisk <amount>, /giveskill <skill> [level], /setstanding <ownerID> <standing>, /spawn, /tr me <place or pilot>";

pub async fn slash(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if call.method != "SlashCmd" {
        return None;
    }
    if ctx.session.role & (ROLE_GML | ROLE_LEGIONEER) == 0 {
        return Some(Outcome::custom("You are not allowed to use slash commands."));
    }
    let line = call.arg_text(0).unwrap_or_default();
    let mut words = line.trim().trim_start_matches('/').split_whitespace();
    let command = words.next().unwrap_or("").to_lowercase();
    let rest: Vec<&str> = words.collect();
    let me = ctx.session.char_id.unwrap_or(0);
    let result = match command.as_str() {
        "help" | "" => Ok(HELP.to_string()),
        "create" | "createitem" => create(ctx, &rest).await,
        "giveisk" | "givemoney" => give_isk(ctx, &rest).await,
        "giveskill" => give_skill(ctx, &rest).await,
        "setstanding" => set_standing(ctx, &rest).await,
        "spawn" => spawn(ctx),
        "tr" => teleport(ctx, &rest).await,
        other => Err(format!("Unknown command /{other}. {HELP}")),
    };
    info!("{me} /{command} {}: {}", rest.join(" "), match &result { Ok(r) => r.as_str(), Err(e) => e.as_str() });
    Some(match result {
        Ok(reply) => Outcome::Value(Value::text(reply)),
        Err(why) => Outcome::custom(why),
    })
}

/// A type by ID, or by its name (exactly, else the first that starts so).
async fn find_type(db: &impl GenericClient, words: &[&str], category: Option<i32>) -> Option<(i64, String)> {
    let key = words.join(" ");
    let sql = r#"SELECT t."typeID", t."typeName" FROM "invTypes" t JOIN "invGroups" g ON g."groupID" = t."groupID"
                  WHERE ($3::int IS NULL OR g."categoryID" = $3)
                    AND (t."typeID"::text = $1 OR lower(t."typeName") = lower($1) OR lower(t."typeName") LIKE lower($2))
                  ORDER BY (t."typeID"::text = $1) DESC, (lower(t."typeName") = lower($1)) DESC, length(t."typeName")
                  LIMIT 1"#;
    let r = db.query_opt(sql, &[&key, &format!("{key}%"), &category]).await.ok().flatten()?;
    Some((int(&r, 0)?, text(&r, 1).unwrap_or_default()))
}

/// The trailing number of a command, if there is one, and the words before.
fn trailing_number<'a>(words: &'a [&'a str]) -> (&'a [&'a str], Option<f64>) {
    match words.split_last() {
        Some((last, before)) if !before.is_empty() => match last.replace(',', "").parse::<f64>() {
            Ok(n) => (before, Some(n)),
            Err(_) => (words, None),
        },
        _ => (words, None),
    }
}

async fn create(ctx: &Ctx<'_>, words: &[&str]) -> Result<String, String> {
    let me = ctx.session.char_id.unwrap_or(0);
    let (name, quantity) = trailing_number(words);
    let quantity = quantity.unwrap_or(1.0).max(1.0) as i64;
    let (type_id, type_name) = find_type(ctx.db, name, None).await.ok_or("No such type.")?;
    let (location, flag) = match (ctx.session.station_id, ctx.session.ship_id) {
        (Some(station), _) => (station, FLAG_HANGAR),
        (None, Some(ship)) => (ship, FLAG_CARGO),
        _ => return Err("You have nowhere to put it.".into()),
    };
    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| e.to_string())?;
    let mut txn = ItemTxn::begin(&mut client).await.map_err(|e| e.to_string())?;
    txn.create(NewItem { type_id, owner_id: me, location_id: location, flag, quantity, singleton: false })
        .await
        .map_err(|e| e.to_string())?;
    let events = txn.commit().await.map_err(|e| e.to_string())?;
    crate::items::notify(ctx.world, ctx.client_id, events);
    Ok(format!("Created {quantity} x {type_name}"))
}

async fn give_isk(ctx: &Ctx<'_>, words: &[&str]) -> Result<String, String> {
    let me = ctx.session.char_id.unwrap_or(0);
    let amount: f64 = words.first().and_then(|w| w.replace(',', "").parse().ok()).ok_or("How much?")?;
    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| e.to_string())?;
    let tx = client.transaction().await.map_err(|e| e.to_string())?;
    let entry = Entry { ref_type: REF_GM, payer: OWNER_SYSTEM, payee: me, arg_id1: "", reason: "GM" };
    let balance = wallet::post(&tx, me, amount, &entry).await.map_err(|_| "The wallet would go below zero.".to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    let w = ctx.world.lock();
    let cash = Value::Tuple(vec![Value::bytes("cash"), Value::Int(me), Value::Double(balance)]);
    w.notify(&w.clients_of(me), "OnAccountChange", cash);
    Ok(format!("Your balance is now {balance:.2} ISK"))
}

/// A trained skill, made in the character (or an existing one raised).
/// The skill list shows it after the next login.
async fn give_skill(ctx: &Ctx<'_>, words: &[&str]) -> Result<String, String> {
    let me = ctx.session.char_id.unwrap_or(0);
    let (name, level) = trailing_number(words);
    let level = level.unwrap_or(5.0).clamp(0.0, 5.0) as i64;
    let (type_id, skill_name) = find_type(ctx.db, name, Some(CATEGORY_SKILL)).await.ok_or("No such skill.")?;
    let stc = ctx
        .query_one(
            r#"SELECT COALESCE("valueFloat", "valueInt"::float8) FROM "dgmTypeAttributes" WHERE "typeID" = $1 AND "attributeID" = $2"#,
            &[&(type_id as i32), &ATTR_SKILL_TIME_CONSTANT],
        )
        .await
        .and_then(|r| float(&r, 0))
        .unwrap_or(1.0)
        * 250.0;
    let points = crate::losses::points_at(stc, level);
    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| e.to_string())?;
    let mut txn = ItemTxn::begin(&mut client).await.map_err(|e| e.to_string())?;
    let held = r#"SELECT "itemID" FROM entity WHERE "locationID" = $1 AND "typeID" = $2 AND "flag" = $3"#;
    let existing = txn
        .tx()
        .query_opt(held, &[&(me as i32), &(type_id as i32), &(FLAG_SKILL as i32)])
        .await
        .map_err(|e| e.to_string())?
        .and_then(|r| int(&r, 0));
    let id = match existing {
        Some(id) => id,
        None => txn
            .create(NewItem { type_id, owner_id: me, location_id: me, flag: FLAG_SKILL, quantity: 1, singleton: true })
            .await
            .map_err(|e| e.to_string())?,
    };
    let set = r#"INSERT INTO entity_attributes ("itemID", "attributeID", "valueInt", "valueFloat") VALUES ($1, $2, $3, NULL)
                 ON CONFLICT ("itemID", "attributeID") DO UPDATE SET "valueInt" = $3, "valueFloat" = NULL"#;
    for (attr, value) in [(ATTR_SKILL_LEVEL, level), (ATTR_SKILL_POINTS, points.round() as i64)] {
        txn.tx().execute(set, &[&(id as i32), &attr, &value]).await.map_err(|e| e.to_string())?;
    }
    let events = txn.commit().await.map_err(|e| e.to_string())?;
    crate::items::notify(ctx.world, ctx.client_id, events);
    Ok(format!("{skill_name} is now at level {level} ({points:.0} SP); log in again to see it in the skill list"))
}

async fn set_standing(ctx: &Ctx<'_>, words: &[&str]) -> Result<String, String> {
    let me = ctx.session.char_id.unwrap_or(0);
    let from: i64 = words.first().and_then(|w| w.parse().ok()).ok_or("Whose standing?")?;
    let standing: f64 = words.get(1).and_then(|w| w.parse().ok()).ok_or("What standing?")?;
    let standing = standing.clamp(-10.0, 10.0);
    let sql = r#"INSERT INTO "chrNPCStandings" ("characterID", "fromID", "standing") VALUES ($1, $2, $3)
                 ON CONFLICT ("characterID", "fromID") DO UPDATE SET "standing" = $3"#;
    if !ctx.execute(sql, &[&(me as i32), &(from as i32), &standing]).await {
        return Err("The standing could not be saved.".into());
    }
    let w = ctx.world.lock();
    w.notify(&w.clients_of(me), "OnStandingSet", Value::Tuple(vec![Value::Int(from), Value::Int(me), Value::Double(standing)]));
    Ok(format!("{from}'s standing toward you is now {standing:.2}"))
}

fn spawn(ctx: &Ctx<'_>) -> Result<String, String> {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return Err("You have to be in space.".into());
    };
    let mut w = ctx.world.lock();
    let park = w.parks.get_mut(&system).ok_or("There is no park here.")?;
    let at = park.ships.get(&ship_id).map(|s| s.pos).ok_or("Your ship is not here.")?;
    let security = park.wildlife.security;
    let rats = crate::npc::spawn_group(park, ship_id, at + Vec3::new(15_000.0, 0.0, 15_000.0), security, crate::combat::now_ms());
    let entries: Vec<Value> = rats.iter().flat_map(|id| crate::svc::space::add_ball_entries(park, *id)).collect();
    crate::svc::space::broadcast(&w, system, None, entries);
    Ok(format!("{} rat(s) spawned", rats.len()))
}

/// /tr me <where>: a station docks you there; a system, a celestial in one or
/// another pilot's ship puts you in space beside it (from space only).
async fn teleport(ctx: &Ctx<'_>, words: &[&str]) -> Result<String, String> {
    let target = match words {
        ["me", rest @ ..] if !rest.is_empty() => rest.join(" "),
        _ => return Err("Usage: /tr me <station, system, celestial or pilot>".into()),
    };
    let place = r#"SELECT d."itemID", d."groupID", COALESCE(d."solarSystemID", d."itemID"), s."constellationID", s."regionID",
                          d."x", d."y", d."z", COALESCE(d."radius", 0), d."itemName"
                     FROM "mapDenormalize" d
                     JOIN "mapSolarSystems" s ON s."solarSystemID" = COALESCE(d."solarSystemID", d."itemID")
                    WHERE d."itemID"::text = $1 OR lower(d."itemName") = lower($1)
                    ORDER BY (d."itemID"::text = $1) DESC LIMIT 1"#;
    let pilot = r#"SELECT "characterID" FROM "character_" WHERE "characterID"::text = $1 OR lower("characterName") = lower($1) LIMIT 1"#;
    let me_in_space = ctx.session.solar_system_id.is_some();
    let (station, system, constellation, region, pos, name) = if let Some(r) = ctx.query_one(place, &[&target]).await {
        let i = |n| int(&r, n).unwrap_or(0);
        let at = Vec3::new(float(&r, 5).unwrap_or(0.0), float(&r, 6).unwrap_or(0.0), float(&r, 7).unwrap_or(0.0));
        let radius = float(&r, 8).unwrap_or(0.0);
        let station = (i(1) == 15).then_some(i(0));
        // Off a celestial's surface; to a system's sun, an AU out.
        let pos = if i(0) == i(2) { Vec3::new(1.5e11, 0.0, 0.0) } else { at + Vec3::new(radius + 10_000.0, 0.0, 0.0) };
        (station, i(2), i(3), i(4), pos, text(&r, 9).unwrap_or_default())
    } else if let Some(other) = ctx.query_one(pilot, &[&target]).await.and_then(|r| int(&r, 0)) {
        let presence = ctx.world.lock().presence_of(other).ok_or("That pilot is not online.")?;
        let sql = r#"SELECT "constellationID", "regionID" FROM "mapSolarSystems" WHERE "solarSystemID" = $1"#;
        let r = ctx.query_one(sql, &[&(presence.solar_system_id as i32)]).await.ok_or("That pilot is nowhere.")?;
        let pos = {
            let w = ctx.world.lock();
            w.parks
                .get(&presence.solar_system_id)
                .and_then(|p| p.ships.values().find(|s| s.pilot.char_id == other))
                .map_or(Vec3::new(1.5e11, 0.0, 0.0), |s| s.pos + Vec3::new(2_000.0, 0.0, 0.0))
        };
        let station = presence.station_id;
        (station, presence.solar_system_id, int(&r, 0).unwrap_or(0), int(&r, 1).unwrap_or(0), pos, target.clone())
    } else {
        return Err(format!("Nowhere called {target}."));
    };
    if station.is_none() && !me_in_space {
        return Err("Undock first: from a station, /tr only goes to other stations.".into());
    }
    let w = ctx.world.lock();
    w.push_move(ctx.client_id, Move::Teleport { station_id: station, solar_system_id: system, constellation_id: constellation, region_id: region, pos: (pos.x, pos.y, pos.z) });
    Ok(format!("On your way to {name}"))
}

/// `keeper` and `dungeon`: the dungeon editor and acceleration gates. There
/// are no dungeons, so there is nothing to edit or activate.
pub async fn dungeon_tools(call: &Call, _ctx: &Ctx<'_>) -> Option<Outcome> {
    Some(match call.method.as_str() {
        "GetLevelEditor" => Outcome::Value(Value::None),
        "ActivateAccelerationGate" => Outcome::custom("This gate leads nowhere."),
        "DEGetDungeons" | "DEGetFactions" | "DEGetRooms" => Outcome::Value(Value::List(Vec::new())),
        "EditObject" | "CopyObject" | "RemoveObject" => Outcome::custom("There are no dungeons to edit."),
        _ => return None,
    })
}

/// `benchmark`: SaveSession(session, dbconn), a client's performance dump
/// (benchmarkSvc.py), noted and dropped.
pub async fn benchmark(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if call.method != "SaveSession" {
        return None;
    }
    info!("benchmark session from {:?} not stored", ctx.session.char_id);
    Some(Outcome::Value(Value::None))
}
