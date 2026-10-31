use eve_marshal::Value;
use tokio_postgres::types::ToSql;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::bound;
use crate::svc::rows::{row, rowset};

const CONTAINER_WALLET: i64 = 10001;
const CONTAINER_GLOBAL: i64 = 10002;
const CONTAINER_SOLAR_SYSTEM: i64 = 10003;
pub const CONTAINER_HANGAR: i64 = 10004;
const CONTAINER_SCRAP_HEAP: i64 = 10005;
const CONTAINER_FACTORY: i64 = 10006;
const CONTAINER_BANK: i64 = 10007;
const CONTAINER_RECYCLER: i64 = 10008;
const CONTAINER_OFFICES: i64 = 10009;
const CONTAINER_STATION_CHARACTERS: i64 = 10010;
const CONTAINER_CHARACTER: i64 = 10011;
const CONTAINER_CORP_MARKET: i64 = 10012;

pub const FLAG_HANGAR: i32 = 4;
pub const FLAG_CORP_MARKET: i32 = 62;

const CATEGORY_BLUEPRINT: i32 = 9;

const STATION_ID_MIN: i32 = 60_000_000;
const STATION_ID_MAX: i32 = 64_000_000;

const UNLIMITED_CAPACITY: f64 = 9_000_000_000_000_000.0;

pub const INVITEM_HEADER: &[&str] = &[
    "itemID", "typeID", "ownerID", "locationID", "flag", "contraband", "singleton",
    "quantity", "groupID", "categoryID", "customInfo",
];

pub const INVITEM_SELECT: &str = r#"SELECT e."itemID",e."typeID",e."ownerID",e."locationID",e."flag",
                                       e."contraband",e."singleton",e."quantity",
                                       t."groupID",g."categoryID",e."customInfo"
                                  FROM "entity" e
                                  JOIN "invTypes" t ON t."typeID" = e."typeID"
                                  JOIN "invGroups" g ON g."groupID" = t."groupID"
                                 WHERE "#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Special { container_id: i64, owner_id: Option<i64> },
    Item(i64),
}

const BROKER: &str = "invbroker";

const CONTAINER: &str = "invcontainer";

pub async fn invbroker(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    Some(broker_method(ctx, &call.method, &call.args))
}

pub async fn invcontainer(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let container = container_from_params(&bound::params_of(call));
    Some(container_method(ctx, container, call).await)
}

fn broker_method(ctx: &Ctx<'_>, method: &str, args: &[Value]) -> Outcome {
    let arg = |i: usize| args.get(i).and_then(|v| v.as_i64());
    match method {
        "GetInventory" => Outcome::Value(bound::object_ref(
            ctx.node_id,
            CONTAINER,
            &[KIND_SPECIAL, arg(0).unwrap_or(0), arg(1).unwrap_or(NO_OWNER)],
        )),
        "GetInventoryFromId" | "GetInventoryFromIdEx" => Outcome::Value(bound::object_ref(
            ctx.node_id,
            CONTAINER,
            &[KIND_ITEM, arg(0).unwrap_or(0), NO_OWNER],
        )),
        _ => Outcome::Unknown(format!("{BROKER}::{method}")),
    }
}

async fn container_method(ctx: &Ctx<'_>, container: Container, call: &Call) -> Outcome {
    if let Some(outcome) = crate::svc::moves::container_write(ctx, container, call).await {
        return outcome;
    }
    let (method, args) = (call.method.as_str(), &call.args);
    let arg = |i: usize| args.get(i).and_then(|v| v.as_i64());
    // A starbase structure shows its contents only to those its settings let look.
    if let Container::Item(id) = container {
        use crate::svc::starbase::{may, Access};
        if method == "List" && may(ctx, id, Access::ViewPut).await == Some(false) {
            return Outcome::custom("You are not allowed to look inside that structure from here.");
        }
    }
    let out = match method {
        "List" => list(ctx, container).await,

        "ListStations" => {
            list_stations(ctx, arg(0).unwrap_or(0) != 0, arg(1).unwrap_or(0) != 0).await
        }
        "ListStationItems" => list_station_items(ctx, arg(0).unwrap_or(0), false, false).await,
        "ListStationBlueprintItems" => {
            list_station_items(ctx, arg(0).unwrap_or(0), true, arg(1).unwrap_or(0) != 0).await
        }

        "GetItem" => get_item(ctx, container).await,
        "GetCapacity" => get_capacity(ctx, container, arg(0)).await,

        _ => return Outcome::Unknown(format!("{CONTAINER}::{method} on {container:?}")),
    };
    Outcome::Value(out)
}

const KIND_SPECIAL: i64 = 0;
const KIND_ITEM: i64 = 1;
const NO_OWNER: i64 = -1;

fn container_from_params(params: &[i64]) -> Container {
    match (params.first().copied(), params.get(1).copied(), params.get(2).copied()) {
        (Some(KIND_ITEM), Some(item_id), _) => Container::Item(item_id),
        (Some(KIND_SPECIAL), Some(container_id), owner) => Container::Special {
            container_id,
            owner_id: owner.filter(|o| *o != NO_OWNER),
        },
        _ => Container::Special { container_id: CONTAINER_HANGAR, owner_id: None },
    }
}

fn contents_scope(ctx: &Ctx<'_>, container: Container) -> Option<(String, Vec<i32>)> {
    match container {
        Container::Item(item_id) => {
            Some((r#"e."locationID" = $1"#.to_string(), vec![item_id as i32]))
        }
        Container::Special { container_id, owner_id } => match container_id {
            CONTAINER_HANGAR => {
                let station = ctx.session.station_id?;
                let owner = owner_id.or(ctx.session.char_id)?;
                Some((
                    r#"e."locationID" = $1 AND e."ownerID" = $2"#.to_string(),
                    vec![station as i32, owner as i32],
                ))
            }
            CONTAINER_CORP_MARKET => {
                let station = ctx.session.station_id?;
                let owner = owner_id.or(ctx.session.corp_id)?;
                Some((
                    r#"e."locationID" = $1 AND e."ownerID" = $2 AND e."flag" = $3"#.to_string(),
                    vec![station as i32, owner as i32, FLAG_CORP_MARKET],
                ))
            }
            CONTAINER_GLOBAL => {
                let owner = ctx.session.char_id?;
                Some((r#"e."ownerID" = $1"#.to_string(), vec![owner as i32]))
            }
            CONTAINER_CHARACTER => {
                let char_id = ctx.session.char_id?;
                Some((r#"e."locationID" = $1"#.to_string(), vec![char_id as i32]))
            }
            CONTAINER_WALLET
            | CONTAINER_SOLAR_SYSTEM
            | CONTAINER_SCRAP_HEAP
            | CONTAINER_FACTORY
            | CONTAINER_BANK
            | CONTAINER_RECYCLER
            | CONTAINER_OFFICES
            | CONTAINER_STATION_CHARACTERS => None,
            _ => None,
        },
    }
}

pub async fn list(ctx: &Ctx<'_>, container: Container) -> Value {
    let Some((where_sql, vals)) = contents_scope(ctx, container) else {
        return rowset(INVITEM_HEADER, Vec::new());
    };
    let sql = format!("{}{}", INVITEM_SELECT, where_sql);
    query_items(ctx, &sql, &vals).await
}

pub async fn list_station_items(
    ctx: &Ctx<'_>,
    station_id: i64,
    blueprints_only: bool,
    for_corp: bool,
) -> Value {
    let owner = match if for_corp { ctx.session.corp_id } else { ctx.session.char_id } {
        Some(o) => o,
        None => return rowset(INVITEM_HEADER, Vec::new()),
    };
    let mut where_sql = String::from(
        r#"e."ownerID" = $1
             AND (e."locationID" = $2
                  OR e."locationID" IN (SELECT c."itemID" FROM "entity" c
                                         WHERE c."locationID" = $2 AND c."ownerID" = $1))"#,
    );
    let mut vals = vec![owner as i32, station_id as i32];
    if blueprints_only {
        where_sql.push_str(r#" AND g."categoryID" = $3"#);
        vals.push(CATEGORY_BLUEPRINT);
    }
    let sql = format!("{}{}", INVITEM_SELECT, where_sql);
    query_items(ctx, &sql, &vals).await
}

pub async fn list_stations(ctx: &Ctx<'_>, blueprints_only: bool, for_corp: bool) -> Value {
    const HEADER: &[&str] = &["stationID", "itemCount"];
    let owner = match if for_corp { ctx.session.corp_id } else { ctx.session.char_id } {
        Some(o) => o,
        None => return rowset(HEADER, Vec::new()),
    };
    let mut vals: Vec<i32> = vec![owner as i32, STATION_ID_MIN, STATION_ID_MAX];
    let sql = if blueprints_only {
        vals.push(CATEGORY_BLUEPRINT);
        r#"SELECT e."locationID", count(*)::int AS "itemCount"
             FROM "entity" e
             JOIN "invTypes" t ON t."typeID" = e."typeID"
             JOIN "invGroups" g ON g."groupID" = t."groupID"
            WHERE e."ownerID" = $1 AND e."locationID" >= $2 AND e."locationID" < $3
              AND g."categoryID" = $4
            GROUP BY e."locationID""#
    } else {
        r#"SELECT e."locationID", count(*)::int AS "itemCount"
             FROM "entity" e
            WHERE e."ownerID" = $1 AND e."locationID" >= $2 AND e."locationID" < $3
            GROUP BY e."locationID""#
    };
    let params: Vec<&(dyn ToSql + Sync)> = vals.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
    match ctx.db.query(sql, &params).await {
        Ok(rows) => {
            let lines = rows
                .iter()
                .map(|r| Value::List(vec![int_at(r, 0), int_at(r, 1)]))
                .collect();
            rowset(HEADER, lines)
        }
        Err(e) => {
            error!("invbroker ListStations: {err}", err = crate::logging::chain(&e));
            rowset(HEADER, Vec::new())
        }
    }
}

pub async fn get_item(ctx: &Ctx<'_>, container: Container) -> Value {
    let item_id = match container {
        Container::Item(id) => id,
        Container::Special { .. } => match ctx.session.station_id {
            Some(s) => s,
            None => return row(INVITEM_HEADER, vec![Value::None; INVITEM_HEADER.len()]),
        },
    };
    let sql = format!("{}{}", INVITEM_SELECT, r#"e."itemID" = $1"#);
    match ctx.db.query(&sql, &[&(item_id as i32)]).await {
        Ok(rows) => match rows.first() {
            Some(r) => row(INVITEM_HEADER, invitem_cells(r)),
            None => row(INVITEM_HEADER, vec![Value::None; INVITEM_HEADER.len()]),
        },
        Err(e) => {
            error!("invbroker GetItem: {err}", err = crate::logging::chain(&e));
            row(INVITEM_HEADER, vec![Value::None; INVITEM_HEADER.len()])
        }
    }
}

pub async fn get_capacity(ctx: &Ctx<'_>, container: Container, flag: Option<i64>) -> Value {
    const HEADER: &[&str] = &["capacity", "used"];

    let capacity = match container {
        Container::Item(item_id) => {
            let sql = r#"SELECT t."capacity" FROM "entity" e
                           JOIN "invTypes" t ON t."typeID" = e."typeID"
                          WHERE e."itemID" = $1"#;
            match ctx.db.query(sql, &[&(item_id as i32)]).await {
                Ok(rows) => rows
                    .first()
                    .and_then(|r| r.try_get::<_, f64>(0).ok())
                    .unwrap_or(0.0),
                Err(e) => {
                    error!("invbroker GetCapacity capacity: {err}", err = crate::logging::chain(&e));
                    0.0
                }
            }
        }
        Container::Special { .. } => UNLIMITED_CAPACITY,
    };

    let used = match contents_scope(ctx, container) {
        Some((mut where_sql, mut vals)) => {
            if let Some(f) = flag {
                where_sql.push_str(&format!(r#" AND e."flag" = ${}"#, vals.len() + 1));
                vals.push(f as i32);
            }
            let sql = format!(
                r#"SELECT COALESCE(SUM(t."volume" * e."quantity"), 0)
                     FROM "entity" e
                     JOIN "invTypes" t ON t."typeID" = e."typeID"
                    WHERE {where_sql}"#
            );
            let params: Vec<&(dyn ToSql + Sync)> =
                vals.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
            match ctx.db.query(&sql, &params).await {
                Ok(rows) => rows
                    .first()
                    .and_then(|r| r.try_get::<_, f64>(0).ok())
                    .unwrap_or(0.0),
                Err(e) => {
                    error!("invbroker GetCapacity used: {err}", err = crate::logging::chain(&e));
                    0.0
                }
            }
        }
        None => 0.0,
    };

    row(HEADER, vec![Value::Double(capacity), Value::Double(used)])
}

async fn query_items(ctx: &Ctx<'_>, sql: &str, vals: &[i32]) -> Value {
    let params: Vec<&(dyn ToSql + Sync)> = vals.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
    match ctx.db.query(sql, &params).await {
        Ok(rows) => {
            let lines = rows.iter().map(|r| Value::List(invitem_cells(r))).collect();
            rowset(INVITEM_HEADER, lines)
        }
        Err(e) => {
            error!("invbroker item listing: {err}", err = crate::logging::chain(&e));
            rowset(INVITEM_HEADER, Vec::new())
        }
    }
}

/// Column positions in an item row (the client's const.ix*).
pub const IX_LOCATION_ID: i64 = 3;
pub const IX_QUANTITY: i64 = 7;

/// `OnItemChange(item, change)` arguments: the item as it is now and, keyed by
/// column position, the values it had before (invCache.py:111). An item whose
/// quantity is now 0 is gone; one whose old location is 0 is new.
pub fn item_change(cells: Vec<Value>, before: Vec<(i64, Value)>) -> Value {
    Value::Tuple(vec![
        row(INVITEM_HEADER, cells),
        Value::Dict(before.into_iter().map(|(ix, v)| (Value::Int(ix), v)).collect()),
    ])
}

pub fn invitem_cells(r: &tokio_postgres::Row) -> Vec<Value> {
    let mut cells: Vec<Value> = (0..10).map(|i| int_at(r, i)).collect();
    cells.push(match r.try_get::<_, Option<String>>(10) {
        Ok(Some(s)) => Value::Bytes(s.into_bytes()),
        _ => Value::None,
    });
    cells
}

fn int_at(r: &tokio_postgres::Row, i: usize) -> Value {
    match r.try_get::<_, i32>(i) {
        Ok(v) => Value::Int(v as i64),
        Err(_) => Value::None,
    }
}


