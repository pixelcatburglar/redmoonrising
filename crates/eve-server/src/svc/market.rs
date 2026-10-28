use std::collections::BTreeMap;

use eve_marshal::Value;
use tokio_postgres::Row;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{cell, row, rowset_with_columns};

pub async fn market_proxy(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let arg0 = || call.arg_id(0);

    let out = match call.method.as_str() {
        "GetMarketGroups" => Outcome::Value(market_groups(ctx).await),

        "GetOrders" => Outcome::Value(orders_for_type(ctx, arg0()).await),

        "GetOldPriceHistory" => {
            Outcome::Value(Value::Tuple(vec![Value::None, Value::List(Vec::new())]))
        }
        "GetNewPriceHistory" => Outcome::Value(price_history(ctx, arg0()).await),

        "GetStationAsks" => Outcome::Value(best_asks(ctx, Scope::Station).await),
        "GetSystemAsks" => Outcome::Value(best_asks(ctx, Scope::SolarSystem).await),
        "GetRegionBest" => Outcome::Value(best_asks(ctx, Scope::Region).await),

        "GetCharOrders" => Outcome::Value(char_orders(ctx).await),
        "GetCorporationOrders" => Outcome::Value(corporation_orders(ctx).await),

        "PlaceCharOrder" | "CancelCharOrder" | "ModifyCharOrder" => {
            return crate::svc::trade::market_write(call, ctx).await;
        }
        "GetCharSkillLimits" => {
            Outcome::Unknown("marketProxy::GetCharSkillLimits has no client call site".into())
        }

        _ => return None,
    };
    Some(out)
}



const GROUP_HEADER: &[&str] = &[
    "marketGroupID",
    "parentGroupID",
    "marketGroupName",
    "description",
    "graphicID",
    "hasTypes",
    "types",
];

async fn market_groups(ctx: &Ctx<'_>) -> Value {
    let types_sql = r#"WITH RECURSIVE "sub"("rootID","nodeID","depth") AS (
                           SELECT "marketGroupID","marketGroupID",0 FROM "invMarketGroups"
                         UNION ALL
                           SELECT s."rootID", g."marketGroupID", s."depth" + 1
                             FROM "sub" s
                             JOIN "invMarketGroups" g ON g."parentGroupID" = s."nodeID"
                            WHERE s."depth" < 12
                       )
                       SELECT s."rootID", t."typeID"
                         FROM "sub" s
                         JOIN "invTypes" t ON t."marketGroupID" = s."nodeID"
                        WHERE t."published" = 1
                        ORDER BY s."rootID", t."typeID""#;

    let mut types_by_group: BTreeMap<i32, Vec<Value>> = BTreeMap::new();
    match ctx.db.query(types_sql, &[]).await {
        Ok(rows) => {
            for r in &rows {
                let group: i32 = r.get(0);
                let type_id: i32 = r.get(1);
                types_by_group.entry(group).or_default().push(Value::Int(type_id as i64));
            }
        }
        Err(e) => error!("GetMarketGroups types: {err}", err = crate::logging::chain(&e)),
    }

    let groups_sql = r#"SELECT "marketGroupID","parentGroupID","marketGroupName","description",
                               "graphicID",COALESCE("hasTypes",0)
                          FROM "invMarketGroups" ORDER BY "marketGroupID""#;
    let rows = match ctx.db.query(groups_sql, &[]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("GetMarketGroups: {err}", err = crate::logging::chain(&e));
            return Value::Dict(vec![(Value::None, rowset_with_columns(GROUP_HEADER, Vec::new()))]);
        }
    };

    let mut buckets: BTreeMap<Option<i32>, Vec<Value>> = BTreeMap::new();
    buckets.entry(None).or_default();
    for r in &rows {
        let group_id: i32 = r.get(0);
        let parent: Option<i32> = r.get(1);
        let line = Value::List(vec![
            Value::Int(group_id as i64),
            parent.map(|p| Value::Int(p as i64)).unwrap_or(Value::None),
            cell(r, 2),
            r.get::<_, Option<String>>(3).map(|d| Value::Bytes(d.into_bytes())).unwrap_or(Value::None),
            cell(r, 4),
            cell(r, 5),
            Value::List(types_by_group.remove(&group_id).unwrap_or_default()),
        ]);
        buckets.entry(parent).or_default().push(line);
        buckets.entry(Some(group_id)).or_default();
    }

    Value::Dict(
        buckets
            .into_iter()
            .map(|(parent, lines)| {
                let key = parent.map(|p| Value::Int(p as i64)).unwrap_or(Value::None);
                (key, rowset_with_columns(GROUP_HEADER, lines))
            })
            .collect(),
    )
}

pub const ORDER_HEADER: &[&str] = &[
    "orderID",
    "typeID",
    "charID",
    "regionID",
    "stationID",
    "range",
    "bid",
    "price",
    "volEntered",
    "volRemaining",
    "issued",
    "orderState",
    "minVolume",
    "contraband",
    "accountID",
    "duration",
    "isCorp",
    "solarSystemID",
    "escrow",
    "jumps",
    "constellationID",
];

pub const ORDER_SELECT: &str = r#"SELECT o."orderID",o."typeID",o."charID",o."regionID",o."stationID",
                                     o."range",o."bid",o."price",o."volEntered",o."volRemaining",
                                     o."issued",o."orderState",o."minVolume",o."contraband",
                                     o."accountID",o."duration",o."isCorp",o."solarSystemID",
                                     o."escrow",o."jumps",s."constellationID"
                                FROM "market_orders" o
                                LEFT JOIN "mapSolarSystems" s
                                       ON s."solarSystemID" = o."solarSystemID""#;

pub fn order_line(r: &Row) -> Vec<Value> {
    vec![
        cell(r, 0),
        cell(r, 1),
        cell(r, 2),
        cell(r, 3),
        cell(r, 4),
        cell(r, 5),
        cell(r, 6),
        cell(r, 7),
        cell(r, 8),
        cell(r, 9),
        cell(r, 10),
        cell(r, 11),
        cell(r, 12),
        cell(r, 13),
        cell(r, 14),
        cell(r, 15),
        cell(r, 16),
        cell(r, 17),
        cell(r, 18),
        cell(r, 19),
        cell(r, 20),
    ]
}

async fn orders_for_type(ctx: &Ctx<'_>, type_id: i64) -> Value {
    let empty =
        || Value::Tuple(vec![rowset_with_columns(ORDER_HEADER, Vec::new()), rowset_with_columns(ORDER_HEADER, Vec::new())]);
    let Some(region_id) = ctx.session.region_id else { return empty() };

    let sql = format!(
        r#"{sel} WHERE o."typeID" = $1 AND o."regionID" = $2 AND o."orderState" = 0
                   AND o."volRemaining" > 0
                 ORDER BY o."price", o."issued""#,
        sel = ORDER_SELECT
    );
    let rows = match ctx.db.query(sql.as_str(), &[&(type_id as i32), &(region_id as i32)]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("GetOrders({type_id}): {err}", err = crate::logging::chain(&e));
            return empty();
        }
    };

    let (mut asks, mut bids) = (Vec::new(), Vec::new());
    for r in &rows {
        let bid: i16 = r.get(6);
        let line = Value::List(order_line(r));
        if bid == 0 { asks.push(line) } else { bids.push(line) }
    }
    Value::Tuple(vec![rowset_with_columns(ORDER_HEADER, asks), rowset_with_columns(ORDER_HEADER, bids)])
}

async fn char_orders(ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return rowset_with_columns(ORDER_HEADER, Vec::new()) };
    let sql = format!(
        r#"{sel} WHERE o."charID" = $1 AND o."orderState" = 0 ORDER BY o."issued" DESC"#,
        sel = ORDER_SELECT
    );
    match ctx.db.query(sql.as_str(), &[&(char_id as i32)]).await {
        Ok(rows) => {
            rowset_with_columns(ORDER_HEADER, rows.iter().map(|r| Value::List(order_line(r))).collect())
        }
        Err(e) => {
            error!("GetCharOrders: {err}", err = crate::logging::chain(&e));
            rowset_with_columns(ORDER_HEADER, Vec::new())
        }
    }
}

async fn corporation_orders(ctx: &Ctx<'_>) -> Value {
    let Some(corp_id) = ctx.session.corp_id else { return rowset_with_columns(ORDER_HEADER, Vec::new()) };
    let sql = format!(
        r#"{sel} JOIN "character_" c ON c."characterID" = o."charID"
                 WHERE o."isCorp" = 1 AND o."orderState" = 0 AND c."corporationID" = $1
                 ORDER BY o."issued" DESC"#,
        sel = ORDER_SELECT
    );
    match ctx.db.query(sql.as_str(), &[&(corp_id as i32)]).await {
        Ok(rows) => {
            rowset_with_columns(ORDER_HEADER, rows.iter().map(|r| Value::List(order_line(r))).collect())
        }
        Err(e) => {
            error!("GetCorporationOrders: {err}", err = crate::logging::chain(&e));
            rowset_with_columns(ORDER_HEADER, Vec::new())
        }
    }
}

enum Scope {
    Station,
    SolarSystem,
    Region,
}

async fn best_asks(ctx: &Ctx<'_>, scope: Scope) -> Value {
    let (column, scope_id) = match scope {
        Scope::Station => ("stationID", ctx.session.station_id),
        Scope::SolarSystem => ("solarSystemID", ctx.session.solar_system_id2),
        Scope::Region => ("regionID", ctx.session.region_id),
    };
    let Some(scope_id) = scope_id else { return Value::Dict(Vec::new()) };

    let sql = format!(
        r#"SELECT DISTINCT ON (q."typeID") q.* FROM (
               {sel} WHERE o."bid" = 0 AND o."orderState" = 0 AND o."volRemaining" > 0
                       AND o."{col}" = $1
           ) q ORDER BY q."typeID", q."price", q."issued""#,
        sel = ORDER_SELECT,
        col = column
    );
    match ctx.db.query(sql.as_str(), &[&(scope_id as i32)]).await {
        Ok(rows) => Value::Dict(
            rows.iter()
                .map(|r| {
                    let type_id: i32 = r.get(1);
                    (Value::Int(type_id as i64), row(ORDER_HEADER, order_line(r)))
                })
                .collect(),
        ),
        Err(e) => {
            error!("best asks ({column}): {err}", err = crate::logging::chain(&e));
            Value::Dict(Vec::new())
        }
    }
}

const HISTORY_HEADER: &[&str] = &["historyDate", "lowPrice", "highPrice", "avgPrice", "volume", "orders"];

const FILETIME_DAY: i64 = 864_000_000_000;

async fn price_history(ctx: &Ctx<'_>, type_id: i64) -> Value {
    let mut lines: Vec<Value> = Vec::new();

    if let Some(region_id) = ctx.session.region_id {
        let sql = r#"SELECT ("transactionDateTime" / 864000000000)::bigint,
                            min("price")::float8,
                            max("price")::float8,
                            (sum("price" * "quantity") / NULLIF(sum("quantity"),0))::float8,
                            sum("quantity")::bigint,
                            count(*)::bigint
                       FROM "market_transactions"
                      WHERE "typeID" = $1 AND "regionID" = $2
                      GROUP BY 1 ORDER BY 1"#;
        match ctx.db.query(sql, &[&(type_id as i32), &(region_id as i32)]).await {
            Ok(rows) => {
                for r in &rows {
                    let day: i64 = r.get(0);
                    lines.push(row(
                        HISTORY_HEADER,
                        vec![
                            Value::Long(day * FILETIME_DAY),
                            cell(r, 1),
                            cell(r, 2),
                            cell(r, 3),
                            r.get::<_, Option<i64>>(4).map(Value::Int).unwrap_or(Value::Int(0)),
                            r.get::<_, Option<i64>>(5).map(Value::Int).unwrap_or(Value::Int(0)),
                        ],
                    ));
                }
            }
            Err(e) => error!("GetNewPriceHistory({type_id}): {err}", err = crate::logging::chain(&e)),
        }
    }

    if lines.is_empty() {
        let mut price = 0.0f64;
        let sql = r#"SELECT "basePrice","portionSize" FROM "invTypes" WHERE "typeID" = $1"#;
        if let Ok(rows) = ctx.db.query(sql, &[&(type_id as i32)]).await {
            if let Some(r) = rows.first() {
                let base: f64 = r.get(0);
                let portion: i32 = r.get(1);
                price = if portion > 0 { base / portion as f64 } else { base };
            }
        }
        let today = (crate::bulkdata::filetime_now() / FILETIME_DAY) * FILETIME_DAY;
        lines.push(row(
            HISTORY_HEADER,
            vec![
                Value::Long(today),
                Value::Double(price),
                Value::Double(price),
                Value::Double(price),
                Value::Int(0),
                Value::Int(0),
            ],
        ));
    }

    Value::Tuple(vec![Value::None, Value::List(lines)])
}




