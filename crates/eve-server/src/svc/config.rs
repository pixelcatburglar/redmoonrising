use eve_marshal::Value;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{header_of, index_rowset, line, lines, rowset};

const LOCATION_UNIVERSE: i64 = 9;

pub async fn config(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let ids = || id_args(call);
    let arg = |i: usize| call.arg_i64(i);
    let flag = |i: usize| arg(i).unwrap_or(0) != 0;

    let value = match call.method.as_str() {
        "GetOwnersEx" | "GetMultiOwnersEx" => owners(ctx, &ids()).await,
        "GetLocationsEx" | "GetMultiLocationsEx" => locations(ctx, &ids()).await,
        "GetCorpTickerNamesEx" | "GetMultiCorpTickerNamesEx" => {
            keyed(ctx, "corptickernames", CORP_TICKER_SQL, &ids()).await
        }
        "GetAllianceShortNamesEx" | "GetMultiAllianceShortNamesEx" => {
            keyed(ctx, "allianceshortnames", ALLIANCE_SHORTNAME_SQL, &ids()).await
        }
        "GetInvTypesEx" | "GetMultiInvTypesEx" => {
            keyed(ctx, "invtypes", INV_TYPES_SQL, &ids()).await
        }
        "GetInvGroupsEx" | "GetMultiInvGroupsEx" => {
            keyed(ctx, "invgroups", INV_GROUPS_SQL, &ids()).await
        }
        "GetInvCategoriesEx" | "GetMultiInvCategoriesEx" => {
            keyed(ctx, "invcategories", INV_CATEGORIES_SQL, &ids()).await
        }
        "GetInvMetaGroupsEx" | "GetMultiInvMetaGroupsEx" => {
            keyed(ctx, "invmetagroups", INV_METAGROUPS_SQL, &ids()).await
        }
        "GetBlueprintTypeEx" | "GetMultiBlueprintTypeEx" => {
            keyed(ctx, "invbptypes", BPTYPES_SQL, &ids()).await
        }
        "GetGraphicsEx" | "GetMultiGraphicsEx" => {
            keyed(ctx, "evegraphics", GRAPHICS_SQL, &ids()).await
        }
        "GetAssemblyLineTypeEx" | "GetMultiAssemblyLineTypeEx" => {
            keyed(ctx, "ramaltypes", RAMALTYPES_SQL, &ids()).await
        }

        "GetMapObjects" => {
            map_objects(
                ctx,
                arg(0).unwrap_or(LOCATION_UNIVERSE),
                flag(1),
                flag(2),
                flag(3),
                flag(4),
            )
            .await
        }
        "GetMapConnections" => {
            map_connections(ctx, arg(0).unwrap_or(LOCATION_UNIVERSE), flag(1), flag(2), flag(3))
                .await
        }
        "GetMap" => map_of_system(ctx, arg(0).unwrap_or(0)).await,
        "GetMapLandmarks" => landmarks(ctx).await,
        "SetMapLandmarks" => Value::None,
        "GetCelestialStatistic" => celestial_statistic(ctx, arg(0).unwrap_or(0)).await,
        "GetUnits" => units(ctx).await,
        "GetStationSolarSystemsByOwner" => {
            station_systems_by_owner(ctx, arg(0).unwrap_or(0)).await
        }

        _ => return None,
    };
    Some(Outcome::Value(value))
}

fn id_args(call: &Call) -> Vec<i32> {
    let mut out = Vec::new();
    let mut push = |v: &Value| {
        if let Some(n) = v.as_i64() {
            if let Ok(n) = i32::try_from(n) {
                out.push(n);
            }
        }
    };
    match call.args.first() {
        Some(Value::List(v)) | Some(Value::Tuple(v)) => v.iter().for_each(&mut push),
        Some(v) => push(v),
        None => {}
    }
    out.sort_unstable();
    out.dedup();
    out
}

async fn keyed(ctx: &Ctx<'_>, what: &str, sql: &str, ids: &[i32]) -> Value {
    let stmt = match ctx.db.prepare(sql).await {
        Ok(s) => s,
        Err(e) => {
            error!("config::{what} prepare: {err}", err = crate::logging::chain(&e));
            return Value::None;
        }
    };
    let header = header_of(&stmt);
    match ctx.db.query(&stmt, &[&ids]).await {
        Ok(rows) => Value::Tuple(vec![header, Value::List(lines(&rows))]),
        Err(e) => {
            error!("config::{what}: {err}", err = crate::logging::chain(&e));
            Value::Tuple(vec![header, Value::List(Vec::new())])
        }
    }
}

async fn owners(ctx: &Ctx<'_>, ids: &[i32]) -> Value {
    const SQL: &str = r#"
        SELECT k.id AS "ownerID",
               COALESCE(o."ownerName", c."characterName", co."corporationName") AS "ownerName",
               COALESCE(NULLIF(o."typeID", 0), NULLIF(c."typeID", 0), bt."typeID",
                        CASE WHEN co."corporationID" IS NOT NULL THEN 2 END) AS "typeID"
          FROM unnest($1::int[]) AS k(id)
          LEFT JOIN "eveStaticOwners" o  ON o."ownerID"       = k.id
          LEFT JOIN "character_"     c   ON c."characterID"   = k.id
          LEFT JOIN "bloodlineTypes" bt  ON bt."bloodlineID"  = c."bloodlineID"
          LEFT JOIN "corporation"    co  ON co."corporationID" = k.id
         WHERE COALESCE(o."ownerName", c."characterName", co."corporationName") IS NOT NULL
           AND COALESCE(NULLIF(o."typeID", 0), NULLIF(c."typeID", 0), bt."typeID",
                        CASE WHEN co."corporationID" IS NOT NULL THEN 2 END) IS NOT NULL"#;
    keyed(ctx, "GetMultiOwnersEx", SQL, ids).await
}

async fn locations(ctx: &Ctx<'_>, ids: &[i32]) -> Value {
    const SQL: &str = r#"
        SELECT k.id AS "locationID",
               COALESCE(l."locationName", d."itemName", e."itemName") AS "locationName",
               COALESCE(l."x", d."x", e."x") AS "x",
               COALESCE(l."y", d."y", e."y") AS "y",
               COALESCE(l."z", d."z", e."z") AS "z"
          FROM unnest($1::int[]) AS k(id)
          LEFT JOIN "eveStaticLocations" l ON l."locationID" = k.id
          LEFT JOIN "mapDenormalize"     d ON d."itemID"     = k.id
          LEFT JOIN "entity"             e ON e."itemID"     = k.id
         WHERE COALESCE(l."locationName", d."itemName", e."itemName") IS NOT NULL"#;
    keyed(ctx, "GetMultiLocationsEx", SQL, ids).await
}

const CORP_TICKER_SQL: &str = r#"
    SELECT "corporationID", "tickerName" FROM "corporation"
     WHERE "corporationID" = ANY($1)"#;

const ALLIANCE_SHORTNAME_SQL: &str = r#"
    SELECT "allianceID", "shortName" FROM "alliance_shortnames"
     WHERE "allianceID" = ANY($1)"#;

const INV_TYPES_SQL: &str = r#"
    SELECT "typeID", "groupID", "typeName", "description", "graphicID", "radius", "mass",
           "volume", "capacity", "portionSize", "raceID", "basePrice", "published",
           "marketGroupID", "chanceOfDuplicating"
      FROM "invTypes" WHERE "typeID" = ANY($1)"#;

const INV_GROUPS_SQL: &str = r#"
    SELECT "groupID", "categoryID", "groupName", "description", "graphicID", "useBasePrice",
           "allowManufacture", "allowRecycler", "anchored", "anchorable", "fittableNonSingleton"
      FROM "invGroups" WHERE "groupID" = ANY($1)"#;

const INV_CATEGORIES_SQL: &str = r#"
    SELECT "categoryID", "categoryName", "description", "graphicID"
      FROM "invCategories" WHERE "categoryID" = ANY($1)"#;

const INV_METAGROUPS_SQL: &str = r#"
    SELECT "metaGroupID", "metaGroupName", "description", "graphicID"
      FROM "invMetaGroups" WHERE "metaGroupID" = ANY($1)"#;

const BPTYPES_SQL: &str = r#"
    SELECT "blueprintTypeID", "parentBlueprintTypeID", "productTypeID", "productionTime",
           "techLevel"
      FROM "invBlueprintTypes" WHERE "blueprintTypeID" = ANY($1)"#;

const GRAPHICS_SQL: &str = r#"
    SELECT "graphicID", "url3D", "urlWeb", "description", "published", "obsolete", "icon",
           "urlSound", "explosionID"
      FROM "eveGraphics" WHERE "graphicID" = ANY($1)"#;

const RAMALTYPES_SQL: &str = r#"
    SELECT "assemblyLineTypeID", "assemblyLineTypeName", "description", "baseTimeMultiplier",
           "baseMaterialMultiplier", "volume", "activityID", "minCostPerHour"
      FROM "ramAssemblyLineTypes" WHERE "assemblyLineTypeID" = ANY($1)"#;

const MAP_OBJECT_HEADER: &[&str] = &[
    "itemID", "itemName", "typeID", "groupID", "locationID", "orbitID", "x", "y", "z",
    "factionID", "radius", "security",
];

const REGIONS_SQL: &str = r#"
    SELECT "regionID" AS "itemID", "regionName" AS "itemName", 3 AS "typeID", 3 AS "groupID",
           9 AS "locationID", NULL::int AS "orbitID", "x", "y", "z", "factionID", "radius",
           NULL::float8 AS "security"
      FROM "mapRegions" WHERE $1 = 9 OR "regionID" = $1"#;

const CONSTELLATIONS_SQL: &str = r#"
    SELECT "constellationID" AS "itemID", "constellationName" AS "itemName", 4 AS "typeID",
           4 AS "groupID", "regionID" AS "locationID", NULL::int AS "orbitID", "x", "y", "z",
           "factionID", "radius", NULL::float8 AS "security"
      FROM "mapConstellations" WHERE $1 = 9 OR "regionID" = $1"#;

const SYSTEMS_SQL: &str = r#"
    SELECT "solarSystemID" AS "itemID", "solarSystemName" AS "itemName", 5 AS "typeID",
           5 AS "groupID", "constellationID" AS "locationID", NULL::int AS "orbitID",
           "x", "y", "z", "factionID", "radius", "security"
      FROM "mapSolarSystems" WHERE $1 = 9 OR "constellationID" = $1"#;

const CELESTIALS_SQL: &str = r#"
    SELECT "itemID", "itemName", "typeID", "groupID", "solarSystemID" AS "locationID",
           "orbitID", "x", "y", "z", NULL::int AS "factionID", "radius", "security"
      FROM "mapDenormalize" WHERE $1 <> 9 AND "solarSystemID" = $1"#;

async fn map_objects(
    ctx: &Ctx<'_>,
    id: i64,
    regions: bool,
    constellations: bool,
    systems: bool,
    celestials: bool,
) -> Value {
    let mut parts: Vec<&str> = Vec::new();
    if regions {
        parts.push(REGIONS_SQL);
    }
    if constellations {
        parts.push(CONSTELLATIONS_SQL);
    }
    if systems {
        parts.push(SYSTEMS_SQL);
    }
    if celestials {
        parts.push(CELESTIALS_SQL);
    }
    if parts.is_empty() {
        return rowset(MAP_OBJECT_HEADER, Vec::new());
    }
    let sql = parts.join(" UNION ALL ");
    let id32 = i32::try_from(id).unwrap_or(0);
    ctx.rowset(MAP_OBJECT_HEADER, sql.as_str(), &[&id32]).await
}

const REGION_JUMPS_SQL: &str = r#"
    (SELECT 0 AS ctype, "fromRegionID", NULL::int AS "fromConstellationID",
            NULL::int AS "fromSolarSystemID", NULL::int AS "stargateID",
            NULL::int AS "celestialID", NULL::int AS "toSolarSystemID",
            NULL::int AS "toConstellationID", "toRegionID"
       FROM "mapRegionJumps" WHERE $1 = 9 OR "fromRegionID" = $1)"#;

const CONSTELLATION_JUMPS_SQL: &str = r#"
    (SELECT 0 AS ctype, "fromRegionID", "fromConstellationID",
            NULL::int AS "fromSolarSystemID", NULL::int AS "stargateID",
            NULL::int AS "celestialID", NULL::int AS "toSolarSystemID",
            "toConstellationID", "toRegionID"
       FROM "mapConstellationJumps" WHERE $1 = 9 OR "fromConstellationID" = $1)"#;

const SYSTEM_JUMPS_SQL: &str = r#"
    (SELECT DISTINCT ON (j."fromSolarSystemID", j."toSolarSystemID")
            0 AS ctype, j."fromRegionID", j."fromConstellationID", j."fromSolarSystemID",
            mj."stargateID", mj."celestialID",
            j."toSolarSystemID", j."toConstellationID", j."toRegionID"
       FROM "mapSolarSystemJumps" j
       LEFT JOIN ("mapJumps" mj
                  JOIN "mapDenormalize" sg ON sg."itemID" = mj."stargateID"
                  JOIN "mapDenormalize" dg ON dg."itemID" = mj."celestialID")
              ON sg."solarSystemID" = j."fromSolarSystemID"
             AND dg."solarSystemID" = j."toSolarSystemID"
      WHERE $1 = 9 OR j."fromSolarSystemID" = $1)"#;

async fn map_connections(ctx: &Ctx<'_>, id: i64, reg: bool, con: bool, sol: bool) -> Value {
    let mut parts: Vec<&str> = Vec::new();
    if reg {
        parts.push(REGION_JUMPS_SQL);
    }
    if con {
        parts.push(CONSTELLATION_JUMPS_SQL);
    }
    if sol {
        parts.push(SYSTEM_JUMPS_SQL);
    }
    if parts.is_empty() {
        return Value::List(Vec::new());
    }
    let sql = parts.join(" UNION ALL ");
    let id32 = i32::try_from(id).unwrap_or(0);
    match ctx.db.query(sql.as_str(), &[&id32]).await {
        Ok(rows) => Value::List(rows.iter().map(|r| Value::Tuple(line(r))).collect()),
        Err(e) => {
            error!("config::GetMapConnections({id}): {err}", err = crate::logging::chain(&e));
            Value::List(Vec::new())
        }
    }
}

async fn map_of_system(ctx: &Ctx<'_>, solar_system_id: i64) -> Value {
    const HEADER: &[&str] = &["itemID", "typeID", "locationID", "x", "y", "z", "itemName"];
    const SQL: &str = r#"
        SELECT "solarSystemID" AS "itemID", 5 AS "typeID",
               "solarSystemID" AS "locationID", "x", "y", "z",
               "solarSystemName" AS "itemName"
          FROM "mapSolarSystems" WHERE "solarSystemID" = $1
        UNION ALL
        SELECT "itemID", "typeID", "solarSystemID" AS "locationID", "x", "y", "z", "itemName"
          FROM "mapDenormalize" WHERE "solarSystemID" = $1"#;
    let id32 = i32::try_from(solar_system_id).unwrap_or(0);
    ctx.rowset(HEADER, SQL, &[&id32]).await
}

async fn landmarks(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &[
        "landmarkID", "landmarkName", "description", "locationID", "x", "y", "z", "radius",
        "graphicID", "importance", "url2d",
    ];
    const SQL: &str = r#"
        SELECT "landmarkID", "landmarkName", "description", "locationID", "x", "y", "z",
               "radius", "graphicID", "importance", "url2d"
          FROM "mapLandmarks" ORDER BY "landmarkID""#;
    ctx.rowset(HEADER, SQL, &[]).await
}

async fn celestial_statistic(ctx: &Ctx<'_>, item_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "celestialID", "temperature", "spectralClass", "luminosity", "age", "life",
        "orbitRadius", "eccentricity", "massDust", "massGas", "fragmented", "density",
        "surfaceGravity", "escapeVelocity", "orbitPeriod", "rotationRate", "locked",
        "pressure", "radius", "mass",
    ];
    const SQL: &str = r#"
        SELECT "celestialID", "temperature", "spectralClass", "luminosity", "age", "life",
               "orbitRadius", "eccentricity", "massDust", "massGas", "fragmented", "density",
               "surfaceGravity", "escapeVelocity", "orbitPeriod", "rotationRate", "locked",
               "pressure", "radius", "mass"
          FROM "mapCelestialStatistics" WHERE "celestialID" = $1"#;
    let id32 = i32::try_from(item_id).unwrap_or(0);
    ctx.rowset(HEADER, SQL, &[&id32]).await
}

async fn units(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["unitID", "unitName", "displayName", "description"];
    const SQL: &str = r#"SELECT "unitID", "unitName", "displayName", "description"
                           FROM "eveUnits" ORDER BY "unitID""#;
    match ctx.db.query(SQL, &[]).await {
        Ok(rows) => index_rowset(
            HEADER,
            "unitID",
            rows.iter()
                .map(|r| {
                    let l = line(r);
                    (l.first().cloned().unwrap_or(Value::None), Value::List(l))
                })
                .collect(),
        ),
        Err(e) => {
            error!("config::GetUnits: {err}", err = crate::logging::chain(&e));
            index_rowset(HEADER, "unitID", Vec::new())
        }
    }
}

async fn station_systems_by_owner(ctx: &Ctx<'_>, owner_id: i64) -> Value {
    const HEADER: &[&str] = &["solarSystemID"];
    const SQL: &str = r#"
        SELECT DISTINCT st."solarSystemID"
          FROM "staStations" st
          LEFT JOIN "entity" e ON e."itemID" = st."stationID"
         WHERE COALESCE(e."ownerID", st."corporationID") = $1
         ORDER BY st."solarSystemID""#;
    let id32 = i32::try_from(owner_id).unwrap_or(0);
    ctx.rowset(HEADER, SQL, &[&id32]).await
}
