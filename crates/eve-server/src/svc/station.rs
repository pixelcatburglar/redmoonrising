use eve_marshal::Value;
use tracing::{error, warn};

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{cell, row, rowset};

pub async fn station(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetStationItemBits" => station_item_bits(ctx).await,
        "GetGuests" => guests(ctx).await,
        _ => return None,
    };
    Some(Outcome::Value(value))
}

pub async fn station_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetStation" => get_station(ctx, call.arg_id(0)).await,
        "GetSolarSystem" => get_solar_system(ctx, call.arg_id(0)).await,
        "GetConstellation" => get_constellation(ctx, call.arg_id(0)).await,
        "GetRegion" => get_region(ctx, call.arg_id(0)).await,
        "GetAllianceSystems" => alliance_systems(ctx).await,
        _ => return None,
    };
    Some(Outcome::Value(value))
}

/// The station-related part of the `map` service.
pub async fn map(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    match call.method.as_str() {
        "GetStationExtraInfo" => Some(Outcome::Value(station_extra_info(ctx).await)),
        _ => None,
    }
}

async fn station_item_bits(ctx: &Ctx<'_>) -> Value {
    let Some(station_id) = ctx.session.station_id else {
        return Value::List(vec![Value::None; 5]);
    };
    let sql = r#"SELECT stt."hangarGraphicID",
                        COALESCE(e."ownerID", st."corporationID") AS "ownerID",
                        st."stationID",
                        COALESCE((SELECT bit_or(os."serviceID")
                                    FROM "staOperationServices" os
                                   WHERE os."operationID"
                                         = COALESCE(st."operationID", stt."operationID")), 0)
                          AS "serviceMask",
                        st."stationTypeID"
                   FROM "staStations" st
                   LEFT JOIN "staStationTypes" stt
                          ON stt."stationTypeID" = st."stationTypeID"
                   LEFT JOIN "entity" e ON e."itemID" = st."stationID"
                  WHERE st."stationID" = $1"#;
    match ctx.db.query(sql, &[&(station_id as i32)]).await {
        Ok(rows) if !rows.is_empty() => Value::List(cells(&rows[0])),
        Ok(_) => {
            warn!("GetStationItemBits: no staStations row for {station_id}");
            Value::List(vec![Value::None; 5])
        }
        Err(e) => {
            error!("GetStationItemBits: {err}", err = crate::logging::chain(&e));
            Value::List(vec![Value::None; 5])
        }
    }
}

async fn guests(ctx: &Ctx<'_>) -> Value {
    let Some(station_id) = ctx.session.station_id else { return Value::List(Vec::new()) };
    let sql = r#"SELECT "characterID" FROM "character_"
                  WHERE "stationID" = $1 AND "accountID" IS NOT NULL"#;
    match ctx.db.query(sql, &[&(station_id as i32)]).await {
        Ok(rows) => Value::List(
            rows.iter()
                .map(|r| match r.try_get::<_, i32>(0) {
                    Ok(id) => Value::Int(id as i64),
                    Err(_) => Value::None,
                })
                .collect(),
        ),
        Err(e) => {
            error!("GetGuests: {err}", err = crate::logging::chain(&e));
            Value::List(Vec::new())
        }
    }
}

async fn get_station(ctx: &Ctx<'_>, station_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "stationID", "security", "dockingCostPerVolume", "maxShipVolumeDockable",
        "officeRentalCost", "operationID", "stationTypeID", "corporationID",
        "solarSystemID", "constellationID", "regionID", "stationName", "x", "y", "z",
        "reprocessingEfficiency", "reprocessingStationsTake", "reprocessingHangarFlag",
        "ownerID", "description", "serviceMask", "hangarGraphicID", "dockingBayGraphicID",
    ];
    let sql = r#"SELECT st."stationID", st."security", st."dockingCostPerVolume",
                        st."maxShipVolumeDockable", st."officeRentalCost", st."operationID",
                        st."stationTypeID", st."corporationID", st."solarSystemID",
                        st."constellationID", st."regionID", st."stationName",
                        st."x", st."y", st."z", st."reprocessingEfficiency",
                        st."reprocessingStationsTake", st."reprocessingHangarFlag",
                        COALESCE(e."ownerID", st."corporationID") AS "ownerID",
                        COALESCE(op."description", '') AS "description",
                        COALESCE((SELECT bit_or(os."serviceID")
                                    FROM "staOperationServices" os
                                   WHERE os."operationID"
                                         = COALESCE(st."operationID", stt."operationID")), 0)
                          AS "serviceMask",
                        stt."hangarGraphicID", stt."dockingBayGraphicID"
                   FROM "staStations" st
                   LEFT JOIN "staStationTypes" stt
                          ON stt."stationTypeID" = st."stationTypeID"
                   LEFT JOIN "staOperations" op
                          ON op."operationID" = COALESCE(st."operationID", stt."operationID")
                   LEFT JOIN "entity" e ON e."itemID" = st."stationID"
                  WHERE st."stationID" = $1"#;
    single_row(ctx, "GetStation", sql, station_id, HEADER).await
}

async fn get_solar_system(ctx: &Ctx<'_>, solar_system_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "solarSystemID", "solarSystemName", "regionID", "constellationID", "x", "y", "z",
        "luminosity", "border", "fringe", "corridor", "hub", "international", "regional",
        "constellation", "security", "factionID", "radius", "sunTypeID", "securityClass",
        "allianceID", "itemID",
    ];
    let sql = r#"SELECT "solarSystemID", "solarSystemName", "regionID", "constellationID",
                        "x", "y", "z", "luminosity", "border", "fringe", "corridor", "hub",
                        "international", "regional", "constellation", "security", "factionID",
                        "radius", "sunTypeID", "securityClass", "allianceID",
                        "solarSystemID" AS "itemID"
                   FROM "mapSolarSystems" WHERE "solarSystemID" = $1"#;
    single_row(ctx, "GetSolarSystem", sql, solar_system_id, HEADER).await
}

async fn get_constellation(ctx: &Ctx<'_>, constellation_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "constellationID", "constellationName", "regionID", "x", "y", "z",
        "xMin", "xMax", "yMin", "yMax", "zMin", "zMax", "factionID", "radius", "itemID",
    ];
    let sql = r#"SELECT "constellationID", "constellationName", "regionID", "x", "y", "z",
                        "xMin", "xMax", "yMin", "yMax", "zMin", "zMax", "factionID", "radius",
                        "constellationID" AS "itemID"
                   FROM "mapConstellations" WHERE "constellationID" = $1"#;
    single_row(ctx, "GetConstellation", sql, constellation_id, HEADER).await
}

async fn get_region(ctx: &Ctx<'_>, region_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "regionID", "regionName", "x", "y", "z",
        "xMin", "xMax", "yMin", "yMax", "zMin", "zMax", "factionID", "radius", "itemID",
    ];
    let sql = r#"SELECT "regionID", "regionName", "x", "y", "z",
                        "xMin", "xMax", "yMin", "yMax", "zMin", "zMax", "factionID", "radius",
                        "regionID" AS "itemID"
                   FROM "mapRegions" WHERE "regionID" = $1"#;
    single_row(ctx, "GetRegion", sql, region_id, HEADER).await
}

async fn alliance_systems(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["solarSystemID", "allianceID"];
    let sql = r#"SELECT "solarSystemID", "allianceID" FROM "mapSolarSystems"
                  WHERE "allianceID" IS NOT NULL"#;
    match ctx.db.query(sql, &[]).await {
        Ok(rows) => rowset(HEADER, rows.iter().map(|r| Value::List(cells(r))).collect()),
        Err(e) => {
            error!("GetAllianceSystems: {err}", err = crate::logging::chain(&e));
            rowset(HEADER, Vec::new())
        }
    }
}

async fn station_extra_info(ctx: &Ctx<'_>) -> Value {
    const STATIONS: &[&str] = &[
        "stationID", "operationID", "stationTypeID", "corporationID", "ownerID",
        "solarSystemID", "constellationID", "regionID", "serviceMask", "security",
        "officeRentalCost", "maxShipVolumeDockable", "dockingCostPerVolume",
        "reprocessingEfficiency", "reprocessingStationsTake", "reprocessingHangarFlag",
    ];
    const OPSERVICES: &[&str] = &["operationID", "serviceID"];
    const SERVICES: &[&str] = &["serviceID", "serviceName", "description"];

    let stations_sql = r#"SELECT st."stationID", st."operationID", st."stationTypeID",
                                 st."corporationID",
                                 COALESCE(e."ownerID", st."corporationID") AS "ownerID",
                                 st."solarSystemID", st."constellationID", st."regionID",
                                 COALESCE((SELECT bit_or(os."serviceID")
                                             FROM "staOperationServices" os
                                            WHERE os."operationID"
                                                  = COALESCE(st."operationID",
                                                             stt."operationID")), 0)
                                   AS "serviceMask",
                                 st."security", st."officeRentalCost",
                                 st."maxShipVolumeDockable", st."dockingCostPerVolume",
                                 st."reprocessingEfficiency", st."reprocessingStationsTake",
                                 st."reprocessingHangarFlag"
                            FROM "staStations" st
                            LEFT JOIN "staStationTypes" stt
                                   ON stt."stationTypeID" = st."stationTypeID"
                            LEFT JOIN "entity" e ON e."itemID" = st."stationID"
                           ORDER BY st."stationID""#;
    let opservices_sql = r#"SELECT "operationID","serviceID" FROM "staOperationServices"
                             ORDER BY "operationID","serviceID""#;
    let services_sql = r#"SELECT "serviceID","serviceName","description" FROM "staServices"
                           ORDER BY "serviceID""#;

    Value::Tuple(vec![
        plain_rowset(ctx, "GetStationExtraInfo/stations", stations_sql, STATIONS).await,
        plain_rowset(ctx, "GetStationExtraInfo/opservices", opservices_sql, OPSERVICES).await,
        plain_rowset(ctx, "GetStationExtraInfo/services", services_sql, SERVICES).await,
    ])
}




async fn single_row(
    ctx: &Ctx<'_>,
    what: &str,
    sql: &str,
    id: i64,
    header: &[&str],
) -> Value {
    match ctx.db.query(sql, &[&(id as i32)]).await {
        Ok(rows) if !rows.is_empty() => row(header, cells(&rows[0])),
        Ok(_) => Value::None,
        Err(e) => {
            error!("{what}: {err}", err = crate::logging::chain(&e));
            Value::None
        }
    }
}

async fn plain_rowset(ctx: &Ctx<'_>, what: &str, sql: &str, header: &[&str]) -> Value {
    match ctx.db.query(sql, &[]).await {
        Ok(rows) => rowset(header, rows.iter().map(|r| Value::List(cells(r))).collect()),
        Err(e) => {
            error!("{what}: {err}", err = crate::logging::chain(&e));
            rowset(header, Vec::new())
        }
    }
}

fn cells(r: &tokio_postgres::Row) -> Vec<Value> {
    (0..r.columns().len()).map(|i| cell(r, i)).collect()
}

