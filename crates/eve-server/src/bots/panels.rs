//! The dashboard's bot windows, put together every save: the bots' market,
//! what their sides feel about players (and who has hired whom), the
//! pilots being followed, and politics' latest upheavals.

use std::collections::HashMap;

use serde_json::{json, Value as Json};
use tokio_postgres::Client;

use super::behaviour::{Atlas, Bot};
use super::control::Following;
use super::economy::{Economy, Kind};
use super::events::Events;
use super::hire::Hires;
use super::presence::Identity;
use super::relations::{Relations, BLUE, BOUNTY, CONTRACT, HOSTILE, WAR};
use crate::svc::rows::{int, text};

/// FILETIME of 1970-01-01, in milliseconds since 1601.
const UNIX_EPOCH_MS: i64 = 11_644_473_600_000;

fn ms(filetime: i64) -> i64 {
    filetime / 10_000 - UNIX_EPOCH_MS
}

/// Names the panels need, read once.
pub struct Names {
    pub systems: HashMap<i32, String>,
    pub stations: HashMap<i32, String>,
}

pub async fn names(db: &Client) -> Names {
    let mut n = Names { systems: HashMap::new(), stations: HashMap::new() };
    if let Ok(rows) = db.query(r#"SELECT "solarSystemID", "solarSystemName" FROM "mapSolarSystems""#, &[]).await {
        n.systems = rows.iter().map(|r| (int(r, 0).unwrap_or(0) as i32, text(r, 1).unwrap_or_default())).collect();
    }
    if let Ok(rows) = db.query(r#"SELECT "stationID", "stationName" FROM "staStations""#, &[]).await {
        n.stations = rows.iter().map(|r| (int(r, 0).unwrap_or(0) as i32, text(r, 1).unwrap_or_default())).collect();
    }
    n
}

impl Names {
    pub fn system(&self, id: i32) -> String {
        self.systems.get(&id).cloned().unwrap_or_else(|| id.to_string())
    }

    pub fn station(&self, id: i32) -> String {
        self.stations.get(&id).cloned().unwrap_or_else(|| id.to_string())
    }
}

/// Owner names (characters, corporations, alliances) for the IDs given.
async fn owners(db: &Client, ids: &[i32]) -> HashMap<i32, String> {
    let sql = r#"SELECT "characterID", "characterName" FROM "character_" WHERE "characterID" = ANY($1)
                 UNION ALL SELECT "corporationID", "corporationName" FROM "corporation" WHERE "corporationID" = ANY($1)
                 UNION ALL SELECT "allianceID", "allianceName" FROM alliances WHERE "allianceID" = ANY($1)"#;
    match db.query(sql, &[&ids]).await {
        Ok(rows) => rows.iter().map(|r| (int(r, 0).unwrap_or(0) as i32, text(r, 1).unwrap_or_default())).collect(),
        Err(_) => HashMap::new(),
    }
}

pub fn market(e: &Economy, names: &Names) -> Json {
    let s = &e.stats;
    let mut hubs: Vec<Json> = Vec::new();
    for &hub in &e.hubs {
        let mut goods: Vec<&(i32, i32, i64, f64)> = e.board.iter().filter(|b| b.0 == hub).collect();
        goods.sort_by(|a, b| {
            let kind = |t: i32| e.goods.get(&t).map_or(3, |g| match g.kind {
                Kind::Mineral => 0,
                Kind::Ore => 1,
                Kind::Hull => 2,
            });
            kind(a.1).cmp(&kind(b.1)).then(b.2.cmp(&a.2))
        });
        let worth: f64 = goods.iter().map(|g| g.2 as f64 * g.3).sum();
        let lines: Vec<Json> = goods
            .iter()
            .filter(|g| g.2 > 0 || e.goods.get(&g.1).is_some_and(|x| x.kind == Kind::Mineral))
            .take(24)
            .map(|g| {
                let good = e.goods.get(&g.1);
                json!({
                    "name": good.map_or_else(|| g.1.to_string(), |x| x.name.clone()),
                    "kind": good.map_or("", |x| match x.kind { Kind::Ore => "ore", Kind::Mineral => "mineral", Kind::Hull => "ship" }),
                    "stock": g.2, "ask": g.3, "bid": e.bid(hub, g.1), "npcAsk": good.map_or(0.0, |x| x.ask),
                })
            })
            .collect();
        hubs.push(json!({ "station": names.station(hub), "worth": worth, "goods": lines }));
    }
    hubs.sort_by(|a, b| b["worth"].as_f64().unwrap_or(0.0).total_cmp(&a["worth"].as_f64().unwrap_or(0.0)));
    let shortages: Vec<Json> = e.shortages().iter().take(10).map(|(t, d)| json!({ "name": e.goods.get(t).map_or_else(|| t.to_string(), |g| g.name.clone()), "wanted": d.round() })).collect();
    json!({
        "minedM3": s.mined_m3, "refinedUnits": s.refined_units, "hauledM3": s.hauled_m3, "hullsBuilt": s.hulls_built,
        "hullsBought": s.hulls_bought, "hullsFromNpcs": s.hulls_bought_from_npcs, "soldToPlayers": s.sold_to_players,
        "boughtFromPlayers": s.bought_from_players, "playerOrdersFilled": s.player_orders_filled, "orders": s.orders,
        "corpTax": s.corp_tax, "hubs": hubs, "shortages": shortages, "log": e.log.iter().rev().take(10).cloned().collect::<Vec<_>>(),
    })
}

pub async fn relations(db: &Client, r: &Relations, hires: &Hires, bots: &[Bot], ids: &[Identity]) -> Json {
    let list = r.standings();
    let mut wanted: Vec<i32> = list.iter().flat_map(|(s, p, ..)| [*s, *p]).collect();
    wanted.extend(hires.by_bot.values().map(|h| h.player as i32));
    wanted.sort_unstable();
    wanted.dedup();
    let names = owners(db, &wanted).await;
    let name = |id: i32| names.get(&id).cloned().unwrap_or_else(|| id.to_string());
    let flags = |f: u32| {
        let mut v = Vec::new();
        for (bit, word) in [(HOSTILE, "hostile"), (BOUNTY, "bounty"), (WAR, "war"), (CONTRACT, "contract"), (BLUE, "blue")] {
            if f & bit != 0 {
                v.push(word);
            }
        }
        v
    };
    let standings: Vec<Json> = list
        .iter()
        .map(|(side, player, heat, favor, f)| json!({ "side": name(*side), "player": name(*player), "heat": (heat * 10.0).round() / 10.0, "favor": (favor * 10.0).round() / 10.0, "flags": flags(*f) }))
        .collect();
    let wingmen: Vec<Json> = hires
        .by_bot
        .iter()
        .map(|(i, h)| json!({ "bot": ids[*i as usize].name, "player": name(h.player as i32), "rate": h.rate, "system": bots[*i as usize].system }))
        .collect();
    let hunting = r.hunting.len();
    json!({ "standings": standings, "wingmen": wingmen, "hunting": hunting, "grudges": r.grudges.len() })
}

pub async fn followed(db: &Client, f: &Following, bots: &[Bot], ids: &[Identity], e: &Economy, atlas: &Atlas, names: &Names) -> Json {
    let mut wanted: Vec<i32> = f.bots.keys().flat_map(|i| [ids[*i as usize].corp, ids[*i as usize].alliance.unwrap_or(0)]).collect();
    wanted.sort_unstable();
    wanted.dedup();
    let owner = owners(db, &wanted).await;
    let mut list: Vec<Json> = f
        .bots
        .iter()
        .map(|(i, (_, history, _))| {
            let (b, id) = (&bots[*i as usize], &ids[*i as usize]);
            let security = atlas.systems.get(&b.system).map_or(0.0, |s| s.security);
            json!({
                "id": b.id, "name": id.name, "role": b.role.name(),
                "corporation": owner.get(&id.corp).cloned().unwrap_or_default(),
                "alliance": id.alliance.and_then(|a| owner.get(&a).cloned()),
                "ship": e.goods.get(&b.hull).map_or_else(|| b.hull.to_string(), |g| g.name.clone()), "tier": b.tier,
                "isk": b.isk.round(), "online": b.online, "activity": b.activity.name(),
                "system": names.system(b.system), "security": (security * 10.0).round() / 10.0,
                "station": (b.station != 0).then(|| names.station(b.station)),
                "history": history.iter().rev().map(|(t, l)| json!({ "time": ms(*t), "text": l })).collect::<Vec<_>>(),
            })
        })
        .collect();
    list.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Json::Array(list)
}

pub fn events(ev: &Events, names: &Names) -> Json {
    let raids: Vec<Json> = ev.raids.iter().map(|r| json!({ "system": names.system(r.system), "raiders": r.raiders.len(), "kills": r.kills, "lost": r.lost })).collect();
    let log: Vec<Json> = ev.log.iter().rev().map(|(t, l)| json!({ "time": ms(*t), "text": l })).collect();
    json!({ "raids": raids, "log": log })
}
