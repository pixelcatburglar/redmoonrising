//! `missionMgr`: pilots' courier missions and escrows (ui/station/missions).
//!
//! A courier mission is made at a station (the bound creator,
//! moniker.GetCourierMissionCreator(stationID)): the items are wrapped in a
//! package (Plastic Wrap), held by EVE System at the pickup station, and the
//! reward is taken from the issuer. Whoever accepts pays the collateral and
//! gets the package in their hangar there; delivering it at the drop-off
//! hands its contents to the issuer and pays the courier the reward and the
//! collateral back. An escrow is a package that stays where it is, sold for
//! its price: the buyer pays the issuer and unwraps it in their own hangar.
//!
//! The issuer may take back what nobody has accepted. What lapses unaccepted
//! goes back to the issuer, reward and all; an accepted mission that lapses
//! undelivered pays the collateral to the issuer, and the courier keeps the
//! package. The client's own rules stand: one package holds at most
//! const.maxCourierMissionPackage m3.

use std::collections::HashMap;

use eve_marshal::Value;
use tokio_postgres::GenericClient;
use tracing::{error, info};

use crate::items::{ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::svc::bound;
use crate::svc::inventory::{invitem_cells, INVITEM_HEADER, INVITEM_SELECT};
use crate::svc::membership::{db_failed, Refused};
use crate::svc::rows::{float, int, key_val, row, rowset, text};
use crate::svc::wallet::{self, Entry, Transfer};

/// The courier package.
pub const TYPE_PLASTIC_WRAP: i64 = 3468;
const OWNER_SYSTEM: i64 = 1;
const FLAG_HANGAR: i64 = 4;
/// const.maxCourierMissionPackage, in m3.
const MAX_PACKAGE: f64 = 20_000.0;
/// refCourierMissionEscrow: the reward and collateral held, and paid out.
const REF_ESCROW: i32 = 23;

const KIND_COURIER: i16 = 1;
const KIND_ESCROW: i16 = 2;

const OUTSTANDING: i16 = 0;
const ACCEPTED: i16 = 1;
const DONE: i16 = 2;
const CANCELLED: i16 = 3;
const LAPSED: i16 = 4;

const COURIER_HEADER: &[&str] = &[
    "missionID", "kind", "state", "availabilityID", "inOrder", "description", "issuerID",
    "assigneeID", "acceptFee", "acceptorID", "dateIssued", "dateExpires", "dateAccepted",
    "dateCompleted", "totalReward", "tracking", "pickupStationID", "craterID", "dropStationID",
    "volume", "pickupSolarSystemID", "pickupRegionID", "dropSolarSystemID", "dropRegionID",
];
const COURIER_SELECT: &str = r#"SELECT "missionID", "kind", "state", "availabilityID", "inOrder", "description", "issuerID",
        "assigneeID", "acceptFee", "acceptorID", "dateIssued", "dateExpires", "dateAccepted", "dateCompleted",
        "totalReward", "tracking", "pickupStationID", "craterID", "dropStationID", "volume", "pickupSolarSystemID",
        "pickupRegionID", "dropSolarSystemID", "dropRegionID"
   FROM "courierMissions""#;

const ESCROW_HEADER: &[&str] = &[
    "escrowID", "price", "stationID", "solarsystemID", "regionID", "issuerID", "assigneeID",
    "acceptorID", "dateExpires", "description",
];

/// What GetManifest lists of a package (missionentry.py Inspect).
const MANIFEST_HEADER: &[&str] = &["itemID", "typeID", "quantity", "damage", "copy", "productivityLevel", "materialLevel", "bpRuns"];

pub async fn mission_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let station = bound::parse(&call.service).and_then(|b| b.params.first().copied());
    settle_lapsed(ctx).await;
    let result = match (call.method.as_str(), station) {
        ("GetCourierOptions", Some(station)) => Ok(courier_options(ctx, station).await),
        ("CreateCourierMission", Some(station)) => create(ctx, call, station).await,
        ("GetAvailableCourierMissions", _) => Ok(available(ctx, call).await),
        ("GetMyCourierMissions", _) => Ok(mine(ctx).await),
        ("GetAvailableEscrows", _) => Ok(escrows(ctx, call).await),
        ("GetManifest", _) => Ok(manifest(ctx, call.arg_id(0)).await),
        ("AcceptCourierMission", _) => accept(ctx, call.arg_id(0)).await,
        ("DeliverCourierPackage", _) => deliver(ctx, call.arg_id(0)).await,
        ("CancelMission", _) => cancel(ctx, call.arg_id(0)).await,
        _ => return None,
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

// ---- listings ----------------------------------------------------------------------------

/// GetCourierOptions(): the caller's items in the station's hangar, as
/// itemList.header and itemList.items {itemID: line} (base_missions.py:369).
async fn courier_options(ctx: &Ctx<'_>, station: i64) -> Value {
    let me = ctx.session.char_id.unwrap_or(0);
    let sql = format!(r#"{INVITEM_SELECT} e."locationID" = $1 AND e."ownerID" = $2 AND e."flag" = $3 ORDER BY e."itemID""#);
    let items = ctx
        .query(sql.as_str(), &[&(station as i32), &(me as i32), &(FLAG_HANGAR as i32)])
        .await
        .iter()
        .map(|r| (Value::Int(int(r, 0).unwrap_or(0)), Value::List(invitem_cells(r))))
        .collect();
    let header = Value::List(INVITEM_HEADER.iter().map(|h| Value::bytes(*h)).collect());
    key_val(vec![("itemList", key_val(vec![("header", header), ("items", Value::Dict(items))]))])
}

/// GetAvailableCourierMissions(maxVolume, maxCollateral, regionID,
/// solarSystemID, stationID): outstanding missions open to the caller, by
/// where they are picked up (None for any).
async fn available(ctx: &Ctx<'_>, call: &Call) -> Value {
    let me = ctx.session.char_id.unwrap_or(0);
    let corp = ctx.session.corp_id.unwrap_or(0);
    let f = |i: usize| call.args.get(i).and_then(|v| v.as_f64());
    let id = |i: usize| call.arg_i64(i).map(|v| v as i32);
    let sql = format!(
        r#"{COURIER_SELECT} WHERE "kind" = $1 AND "state" = $2 AND "dateExpires" > $3
             AND ("assigneeID" IS NULL OR "assigneeID" IN ($4, $5))
             AND ($6::float8 IS NULL OR "volume" <= $6) AND ($7::float8 IS NULL OR "acceptFee" <= $7)
             AND ($8::int IS NULL OR "pickupRegionID" = $8) AND ($9::int IS NULL OR "pickupSolarSystemID" = $9)
             AND ($10::int IS NULL OR "pickupStationID" = $10)
           ORDER BY "missionID""#
    );
    let now = crate::bulkdata::filetime_now();
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 10] =
        [&KIND_COURIER, &OUTSTANDING, &now, &(me as i32), &(corp as i32), &f(0), &f(1), &id(2), &id(3), &id(4)];
    ctx.rowset(COURIER_HEADER, &sql, &params).await
}

/// GetMyCourierMissions(): those the caller issued or carries, still open.
async fn mine(ctx: &Ctx<'_>) -> Value {
    let me = ctx.session.char_id.unwrap_or(0) as i32;
    let sql = format!(
        r#"{COURIER_SELECT} WHERE "kind" = $1 AND "state" IN ($2, $3) AND ("issuerID" = $4 OR "acceptorID" = $4) ORDER BY "missionID""#
    );
    ctx.rowset(COURIER_HEADER, &sql, &[&KIND_COURIER, &OUTSTANDING, &ACCEPTED, &me]).await
}

/// GetAvailableEscrows(maxPrice, regionID, solarSystemID, stationID,
/// assigneeID): (escrows, names), names covering the stations and people.
async fn escrows(ctx: &Ctx<'_>, call: &Call) -> Value {
    let me = ctx.session.char_id.unwrap_or(0);
    let corp = ctx.session.corp_id.unwrap_or(0);
    let id = |i: usize| call.arg_i64(i).map(|v| v as i32);
    let sql = r#"SELECT "missionID", "acceptFee", "pickupStationID", "pickupSolarSystemID", "pickupRegionID", "issuerID",
                        "assigneeID", "acceptorID", "dateExpires", "description"
                   FROM "courierMissions"
                  WHERE "kind" = $1 AND "state" = $2 AND "dateExpires" > $3
                    AND ("assigneeID" IS NULL OR "assigneeID" IN ($4, $5) OR "issuerID" = $4)
                    AND ($6::float8 IS NULL OR "acceptFee" <= $6)
                    AND ($7::int IS NULL OR "pickupRegionID" = $7) AND ($8::int IS NULL OR "pickupSolarSystemID" = $8)
                    AND ($9::int IS NULL OR "pickupStationID" = $9) AND ($10::int IS NULL OR "assigneeID" = $10)
                  ORDER BY "missionID""#;
    let now = crate::bulkdata::filetime_now();
    let max_price = call.args.first().and_then(|v| v.as_f64());
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 10] =
        [&KIND_ESCROW, &OUTSTANDING, &now, &(me as i32), &(corp as i32), &max_price, &id(1), &id(2), &id(3), &id(4)];
    let rows = ctx.query(sql, &params).await;
    let mut ids: Vec<i32> = Vec::new();
    for r in &rows {
        ids.extend([2usize, 5, 6].iter().filter_map(|i| int(r, *i)).map(|v| v as i32));
    }
    let names = r#"SELECT id, COALESCE((SELECT "stationName" FROM "staStations" WHERE "stationID" = id),
                                       (SELECT "characterName" FROM "character_" WHERE "characterID" = id),
                                       (SELECT "corporationName" FROM "corporation" WHERE "corporationID" = id), '')
                     FROM unnest($1::int[]) AS id"#;
    let names: Vec<(Value, Value)> = ctx
        .query(names, &[&ids])
        .await
        .iter()
        .map(|r| (Value::Int(int(r, 0).unwrap_or(0)), Value::text(text(r, 1).unwrap_or_default())))
        .collect();
    let lines = rows.iter().map(|r| Value::List(crate::svc::rows::line(r))).collect();
    key_val(vec![("escrows", rowset(ESCROW_HEADER, lines)), ("names", Value::Dict(names))])
}

/// GetManifest(escrowID): what the package holds.
async fn manifest(ctx: &Ctx<'_>, mission_id: i64) -> Value {
    let sql = r#"SELECT e."itemID", e."typeID", e."quantity", NULL::float8, COALESCE(b."copy", 0),
                        b."productivityLevel", b."materialLevel", b."licensedProductionRunsRemaining"
                   FROM "courierMissions" m
                   JOIN entity e ON e."locationID" = m."craterID"
                   LEFT JOIN "invBlueprints" b ON b."itemID" = e."itemID"
                  WHERE m."missionID" = $1 ORDER BY e."itemID""#;
    ctx.rowset(MANIFEST_HEADER, sql, &[&(mission_id as i32)]).await
}

// ---- making one --------------------------------------------------------------------------

/// CreateCourierMission(items, dropStationID, description, collateral,
/// assignedTo=, tracking=, reward=, expiryDate=) -> (missionID, excluded):
/// excluded as (item, None, (reason,)) for what could not go in.
async fn create(ctx: &Ctx<'_>, call: &Call, station: i64) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let ids: Vec<i64> = call.args.first().and_then(|v| v.seq()).unwrap_or(&[]).iter().filter_map(item_id_of).collect();
    let drop = call.arg_i64(1).unwrap_or(station);
    let description: Option<String> = call.arg_text(2).map(|d| d.chars().take(1_000).collect());
    let collateral = call.args.get(3).and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0);
    let assignee = call.kwarg("assignedTo").and_then(|v| v.as_i64());
    let tracking = call.kwarg("tracking").and_then(|v| v.as_i64()).unwrap_or(0) as i16;
    let reward = call.kwarg("reward").and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0);
    let now = crate::bulkdata::filetime_now();
    let expires = call.kwarg("expiryDate").and_then(|v| v.as_i64()).unwrap_or(now + 24 * 3_600 * 10_000_000);
    let kind = if drop == station { KIND_ESCROW } else { KIND_COURIER };
    if expires <= now {
        return Err(Outcome::custom("The mission would have expired already."));
    }

    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
    let mut txn = ItemTxn::begin(&mut client).await.map_err(|e| db_failed("BEGIN", e))?;
    let places = r#"SELECT s."stationID", s."solarSystemID", s."regionID" FROM "staStations" s WHERE s."stationID" = ANY($1)"#;
    let found: HashMap<i64, (i64, i64)> = txn
        .tx()
        .query(places, &[&vec![station as i32, drop as i32]])
        .await
        .map_err(|e| db_failed("reading the stations", e))?
        .iter()
        .filter_map(|r| Some((int(r, 0)?, (int(r, 1)?, int(r, 2)?))))
        .collect();
    let (Some(&(pickup_system, pickup_region)), Some(&(drop_system, drop_region))) = (found.get(&station), found.get(&drop)) else {
        return Err(Outcome::custom("That station does not exist."));
    };

    let mut taken = Vec::new();
    let mut excluded = Vec::new();
    let mut volume = 0.0;
    for id in ids {
        let Some(item) = txn.item(id).await.map_err(|e| db_failed("reading the items", e))? else { continue };
        let reason = if item.owner_id != me {
            Some("Not yours")
        } else if item.location_id != station || item.flag != FLAG_HANGAR {
            Some("Not in your hangar here")
        } else if item.type_id == TYPE_PLASTIC_WRAP {
            Some("Already a package")
        } else if crate::svc::votes::locked(txn.tx(), item.id).await {
            Some("Locked down")
        } else {
            None
        };
        match reason {
            Some(why) => {
                let line = txn.tx().query_opt(format!(r#"{INVITEM_SELECT} e."itemID" = $1"#).as_str(), &[&(id as i32)]).await.ok().flatten();
                let rec = row(INVITEM_HEADER, line.map(|r| invitem_cells(&r)).unwrap_or_default());
                excluded.push(Value::Tuple(vec![rec, Value::None, Value::Tuple(vec![Value::text(why)])]));
            }
            None => {
                volume += item.volume * item.quantity as f64;
                taken.push(item.id);
            }
        }
    }
    if taken.is_empty() {
        return Err(Outcome::custom("None of those items can go in a package."));
    }
    if volume > MAX_PACKAGE {
        return Err(Outcome::custom(format!("A package holds at most {MAX_PACKAGE} m3; this one would be {volume:.1} m3.")));
    }
    if reward > 0.0 {
        let reason = format!("Courier mission reward held ({drop})");
        let entry = Entry { ref_type: REF_ESCROW, payer: me, payee: OWNER_SYSTEM, arg_id1: "", reason: &reason };
        pay(&txn, me, -reward, &entry).await?;
    }
    let wrap = NewItem { type_id: TYPE_PLASTIC_WRAP, owner_id: OWNER_SYSTEM, location_id: station, flag: FLAG_HANGAR, quantity: 1, singleton: true };
    let crater = txn.create(wrap).await.map_err(|e| db_failed("wrapping the package", e))?;
    for id in &taken {
        txn.move_to(*id, crater, 0).await.map_err(|e| db_failed("wrapping the package", e))?;
    }
    let sql = r#"INSERT INTO "courierMissions" ("kind", "state", "availabilityID", "inOrder", "description", "issuerID",
                     "assigneeID", "acceptFee", "dateIssued", "dateExpires", "totalReward", "tracking", "pickupStationID", "craterID",
                     "dropStationID", "volume", "pickupSolarSystemID", "pickupRegionID", "dropSolarSystemID", "dropRegionID", "price",
                     "regionID", "solarsystemID", "stationID")
                 VALUES ($1, 0, 0, 0, $2, $3, $4, $5, $6, $7, $8, $9,
                         $10, $11, $12, $13, $14, $15, $16, $17, $5, $15, $14, $10)
                 RETURNING "missionID""#;
    let i = |v: i64| v as i32;
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 17] = [
        &kind, &description, &i(me), &assignee.map(i), &collateral, &now, &expires, &reward, &tracking, &i(station), &i(crater),
        &i(drop), &volume, &i(pickup_system), &i(pickup_region), &i(drop_system), &i(drop_region),
    ];
    let mission_id: i32 = txn.tx().query_one(sql, &params).await.map_err(|e| db_failed("recording the mission", e))?.get(0);
    let events = txn.commit().await.map_err(|e| db_failed("COMMIT", e))?;
    crate::items::notify(ctx.world, ctx.client_id, events);
    tell_balance(ctx, me).await;
    let what = if kind == KIND_ESCROW { "escrow" } else { "courier mission" };
    info!("{me} made {what} {mission_id}: {} item(s), {volume:.1} m3, {station} -> {drop}, reward {reward}, collateral {collateral}", taken.len());
    Ok(Value::Tuple(vec![Value::Int(mission_id as i64), Value::List(excluded)]))
}

/// An item ID, or the itemID of an item row.
fn item_id_of(v: &Value) -> Option<i64> {
    if let Some(id) = v.as_i64() {
        return Some(id);
    }
    let Value::Instance { state, .. } = v else { return None };
    let Value::Dict(d) = state.as_ref() else { return None };
    d.iter().find(|(k, _)| *k == Value::bytes("line")).and_then(|(_, l)| l.seq()).and_then(|l| l.first()).and_then(|v| v.as_i64())
}

// ---- taking, delivering, cancelling ------------------------------------------------------

struct Open {
    kind: i16,
    state: i16,
    issuer: i64,
    assignee: Option<i64>,
    acceptor: Option<i64>,
    fee: f64,
    reward: f64,
    pickup: i64,
    drop: i64,
    crater: i64,
}

async fn open_mission(txn: &ItemTxn<'_>, id: i64) -> Refused<Open> {
    let sql = r#"SELECT "kind", "state", "issuerID", "assigneeID", "acceptorID", "acceptFee", "totalReward", "pickupStationID",
                        "dropStationID", "craterID"
                   FROM "courierMissions" WHERE "missionID" = $1 FOR UPDATE"#;
    let r = txn
        .tx()
        .query_opt(sql, &[&(id as i32)])
        .await
        .map_err(|e| db_failed("reading the mission", e))?
        .ok_or_else(|| Outcome::custom("That mission no longer exists."))?;
    Ok(Open {
        kind: int(&r, 0).unwrap_or(0) as i16,
        state: int(&r, 1).unwrap_or(0) as i16,
        issuer: int(&r, 2).unwrap_or(0),
        assignee: int(&r, 3),
        acceptor: int(&r, 4),
        fee: float(&r, 5).unwrap_or(0.0),
        reward: float(&r, 6).unwrap_or(0.0),
        pickup: int(&r, 7).unwrap_or(0),
        drop: int(&r, 8).unwrap_or(0),
        crater: int(&r, 9).unwrap_or(0),
    })
}

/// AcceptCourierMission(missionID): take on a courier mission, or buy an
/// escrow; on one's own escrow, take it back.
async fn accept(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let corp = ctx.session.corp_id.unwrap_or(0);
    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
    let mut txn = ItemTxn::begin(&mut client).await.map_err(|e| db_failed("BEGIN", e))?;
    let m = open_mission(&txn, id).await?;
    if m.state != OUTSTANDING {
        return Err(Outcome::custom("Someone has already taken that."));
    }
    if m.kind == KIND_ESCROW && m.issuer == me {
        return close_unaccepted(ctx, txn, id, &m, CANCELLED).await;
    }
    if m.issuer == me {
        return Err(Outcome::custom("You cannot accept your own mission."));
    }
    if m.assignee.is_some_and(|a| a != me && a != corp) {
        return Err(Outcome::custom("That is meant for someone else."));
    }
    let now = crate::bulkdata::filetime_now();
    let reason = format!("Courier mission {id}");
    if m.kind == KIND_ESCROW {
        // The price goes to the seller; the goods come out of the wrap here.
        let entry = Entry { ref_type: REF_ESCROW, payer: me, payee: m.issuer, arg_id1: "", reason: &reason };
        pay(&txn, me, -m.fee, &entry).await?;
        pay(&txn, m.issuer, m.fee, &entry).await?;
        unwrap(&mut txn, m.crater, me, m.pickup).await?;
        let done = r#"UPDATE "courierMissions" SET "state" = $2, "acceptorID" = $3, "dateAccepted" = $4, "dateCompleted" = $4 WHERE "missionID" = $1"#;
        txn.tx().execute(done, &[&(id as i32), &DONE, &(me as i32), &now]).await.map_err(|e| db_failed("closing the escrow", e))?;
    } else {
        let entry = Entry { ref_type: REF_ESCROW, payer: me, payee: OWNER_SYSTEM, arg_id1: "", reason: &reason };
        if m.fee > 0.0 {
            pay(&txn, me, -m.fee, &entry).await?;
        }
        txn.set_owner(m.crater, me).await.map_err(|e| db_failed("handing over the package", e))?;
        let took = r#"UPDATE "courierMissions" SET "state" = $2, "acceptorID" = $3, "dateAccepted" = $4 WHERE "missionID" = $1"#;
        txn.tx().execute(took, &[&(id as i32), &ACCEPTED, &(me as i32), &now]).await.map_err(|e| db_failed("accepting the mission", e))?;
    }
    let events = txn.commit().await.map_err(|e| db_failed("COMMIT", e))?;
    crate::items::notify(ctx.world, ctx.client_id, events);
    tell_balance(ctx, me).await;
    tell_balance(ctx, m.issuer).await;
    info!("{me} took {} {id}", if m.kind == KIND_ESCROW { "escrow" } else { "courier mission" });
    Ok(Value::None)
}

/// DeliverCourierPackage(missionID): at the drop-off, the package's contents
/// go to the issuer, and the courier is paid.
async fn deliver(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
    let mut txn = ItemTxn::begin(&mut client).await.map_err(|e| db_failed("BEGIN", e))?;
    let m = open_mission(&txn, id).await?;
    if m.state != ACCEPTED || m.acceptor != Some(me) {
        return Err(Outcome::custom("That is not a mission you are carrying."));
    }
    if ctx.session.station_id != Some(m.drop) {
        return Err(Outcome::custom("You have to be docked at the drop-off station."));
    }
    let package = txn.item(m.crater).await.map_err(|e| db_failed("reading the package", e))?;
    if !package.is_some_and(|p| p.location_id == m.drop && p.owner_id == me && p.flag == FLAG_HANGAR) {
        return Err(Outcome::custom("The package has to be in your hangar here."));
    }
    unwrap(&mut txn, m.crater, m.issuer, m.drop).await?;
    let reason = format!("Courier mission {id}");
    let entry = Entry { ref_type: REF_ESCROW, payer: OWNER_SYSTEM, payee: me, arg_id1: "", reason: &reason };
    if m.reward + m.fee > 0.0 {
        pay(&txn, me, m.reward + m.fee, &entry).await?;
    }
    let now = crate::bulkdata::filetime_now();
    let done = r#"UPDATE "courierMissions" SET "state" = $2, "dateCompleted" = $3 WHERE "missionID" = $1"#;
    txn.tx().execute(done, &[&(id as i32), &DONE, &now]).await.map_err(|e| db_failed("closing the mission", e))?;
    let events = txn.commit().await.map_err(|e| db_failed("COMMIT", e))?;
    crate::items::notify(ctx.world, ctx.client_id, events);
    tell_balance(ctx, me).await;
    info!("{me} delivered courier mission {id} to {}", m.drop);
    Ok(Value::None)
}

/// CancelMission(missionID): the issuer takes back what nobody has accepted.
async fn cancel(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
    let txn = ItemTxn::begin(&mut client).await.map_err(|e| db_failed("BEGIN", e))?;
    let m = open_mission(&txn, id).await?;
    if m.issuer != me {
        return Err(Outcome::custom("That is not your mission."));
    }
    if m.state != OUTSTANDING {
        return Err(Outcome::custom("Someone has already accepted that mission."));
    }
    close_unaccepted(ctx, txn, id, &m, CANCELLED).await
}

/// An unaccepted mission ends: the items and the reward go back to the issuer.
async fn close_unaccepted(ctx: &Ctx<'_>, mut txn: ItemTxn<'_>, id: i64, m: &Open, state: i16) -> Refused<Value> {
    unwrap(&mut txn, m.crater, m.issuer, m.pickup).await?;
    if m.reward > 0.0 {
        let reason = format!("Courier mission {id} reward returned");
        let entry = Entry { ref_type: REF_ESCROW, payer: OWNER_SYSTEM, payee: m.issuer, arg_id1: "", reason: &reason };
        pay(&txn, m.issuer, m.reward, &entry).await?;
    }
    let sql = r#"UPDATE "courierMissions" SET "state" = $2, "dateCompleted" = $3 WHERE "missionID" = $1"#;
    txn.tx()
        .execute(sql, &[&(id as i32), &state, &crate::bulkdata::filetime_now()])
        .await
        .map_err(|e| db_failed("closing the mission", e))?;
    let events = txn.commit().await.map_err(|e| db_failed("COMMIT", e))?;
    crate::items::notify(ctx.world, ctx.client_id, events);
    tell_balance(ctx, m.issuer).await;
    info!("mission {id} closed unaccepted ({})", if state == LAPSED { "lapsed" } else { "taken back" });
    Ok(Value::None)
}

/// Missions past their date: unaccepted ones go back to their issuers,
/// accepted ones forfeit the collateral to them.
async fn settle_lapsed(ctx: &Ctx<'_>) {
    let now = crate::bulkdata::filetime_now();
    let sql = r#"SELECT "missionID", "state" FROM "courierMissions" WHERE "state" IN ($1, $2) AND "dateExpires" <= $3 ORDER BY "missionID" LIMIT 50"#;
    for r in ctx.query(sql, &[&OUTSTANDING, &ACCEPTED, &now]).await {
        let id = int(&r, 0).unwrap_or(0);
        let settled = async {
            let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
            let txn = ItemTxn::begin(&mut client).await.map_err(|e| db_failed("BEGIN", e))?;
            let m = open_mission(&txn, id).await?;
            if m.state == OUTSTANDING {
                return close_unaccepted(ctx, txn, id, &m, LAPSED).await.map(|_| ());
            }
            let reason = format!("Courier mission {id} failed: collateral");
            let entry = Entry { ref_type: REF_ESCROW, payer: OWNER_SYSTEM, payee: m.issuer, arg_id1: "", reason: &reason };
            if m.fee > 0.0 {
                pay(&txn, m.issuer, m.fee, &entry).await?;
            }
            let sql = r#"UPDATE "courierMissions" SET "state" = $2, "dateCompleted" = $3 WHERE "missionID" = $1"#;
            txn.tx().execute(sql, &[&(id as i32), &LAPSED, &now]).await.map_err(|e| db_failed("failing the mission", e))?;
            txn.commit().await.map_err(|e| db_failed("COMMIT", e))?;
            info!("courier mission {id} lapsed undelivered; the collateral went to {}", m.issuer);
            Ok::<_, Outcome>(())
        }
        .await;
        if settled.is_err() {
            error!("courier mission {id} could not be settled");
        }
    }
}

/// Takes a package apart: its contents to `owner`'s hangar at `station`.
async fn unwrap(txn: &mut ItemTxn<'_>, crater: i64, owner: i64, station: i64) -> Refused<()> {
    let sql = r#"SELECT "itemID" FROM entity WHERE "locationID" = $1"#;
    let inside: Vec<i64> = txn
        .tx()
        .query(sql, &[&(crater as i32)])
        .await
        .map_err(|e| db_failed("opening the package", e))?
        .iter()
        .filter_map(|r| int(r, 0))
        .collect();
    for id in inside {
        txn.item(id).await.map_err(|e| db_failed("opening the package", e))?;
        txn.move_to(id, station, FLAG_HANGAR).await.map_err(|e| db_failed("opening the package", e))?;
        txn.set_owner(id, owner).await.map_err(|e| db_failed("opening the package", e))?;
    }
    if txn.item(crater).await.map_err(|e| db_failed("opening the package", e))?.is_some() {
        txn.destroy(crater).await.map_err(|e| db_failed("opening the package", e))?;
    }
    Ok(())
}

/// The volume of what a package holds.
pub async fn package_volume(db: &impl GenericClient, crater: i64) -> Result<f64, tokio_postgres::Error> {
    let sql = r#"SELECT coalesce(sum(t."volume" * e."quantity"), 0)::float8 FROM entity e
                   JOIN "invTypes" t ON t."typeID" = e."typeID" WHERE e."locationID" = $1"#;
    Ok(db.query_one(sql, &[&(crater as i32)]).await?.get(0))
}

async fn pay(txn: &ItemTxn<'_>, owner: i64, delta: f64, entry: &Entry<'_>) -> Refused<f64> {
    match wallet::post(txn.tx(), owner, delta, entry).await {
        Ok(b) => Ok(b),
        Err(Transfer::NotEnough { need, have }) => Err(Outcome::not_enough_money(need, have)),
        Err(Transfer::NoSuchOwner) => Err(Outcome::custom("That wallet could not be found.")),
        Err(Transfer::Db(e)) => Err(db_failed("moving the money", e)),
    }
}

async fn tell_balance(ctx: &Ctx<'_>, owner: i64) {
    let sql = r#"SELECT "balance" FROM "character_" WHERE "characterID" = $1"#;
    let Some(balance) = ctx.query_one(sql, &[&(owner as i32)]).await.and_then(|r| float(&r, 0)) else { return };
    let w = ctx.world.lock();
    let cash = Value::Tuple(vec![Value::bytes("cash"), Value::Int(owner), Value::Double(balance)]);
    w.notify(&w.clients_of(owner), "OnAccountChange", cash);
}
