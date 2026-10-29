use std::collections::BTreeMap;

use eve_marshal::Value;
use tracing::{error, warn};

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{index_rowset, line, row};

const SERVICE: &str = "dogmaIM";

const EFFECT_ONLINE: i64 = 16;
const ATTR_IS_ONLINE: i32 = 2;

const CHARGED: [(i32, i32, i32); 2] = [(18, 55, 482), (264, 479, 263)];

const ALLOWED_CATEGORIES: [i32; 7] = [11, 23, 6, 7, 20, 16, 8];
const GROUP_CHARACTER: i32 = 1;

pub async fn dogma_im(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    Some(method_call(ctx, &call.method, &call.args).await)
}

async fn method_call(ctx: &Ctx<'_>, method: &str, args: &[Value]) -> Outcome {
    match method {
        "GetAttributeTypes" => Outcome::Value(attribute_types(ctx).await),

        "GetSkillPrerequisites" => Outcome::Value(Value::List(Vec::new())),

        "ShipGetInfo" => Outcome::Value(ship_info(ctx).await),
        "CharGetInfo" => Outcome::Value(char_info(ctx).await),
        "ItemGetInfo" => Outcome::Value(item_info(ctx, args.first()).await),

        "CheckSendLocationInfo" => Outcome::Value(Value::None),

        // Targeting and module activation are `inflight`'s.
        "LogAttribute" => Outcome::Value(Value::List(Vec::new())),

        _ => Outcome::Unknown(format!("{SERVICE}::{method}")),
    }
}

async fn attribute_types(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &[
        "attributeID", "attributeName", "attributeCategory", "description", "maxAttributeID",
        "attributeIdx", "graphicID", "chargeRechargeTimeID", "defaultValue", "published",
        "displayName", "unitID", "stackable", "highIsGood",
    ];
    const SQL: &str = r#"
        SELECT "attributeID","attributeName","attributeCategory","description","maxAttributeID",
               "attributeIdx","graphicID","chargeRechargeTimeID","defaultValue","published",
               "displayName","unitID","stackable","highIsGood"
          FROM "dgmAttributeTypes" ORDER BY "attributeID""#;
    match ctx.db.query(SQL, &[]).await {
        Ok(rows) => index_rowset(
            HEADER,
            "attributeName",
            rows.iter()
                .map(|r| {
                    let l = line(r);
                    (l.get(1).cloned().unwrap_or(Value::None), Value::List(l))
                })
                .collect(),
        ),
        Err(e) => {
            error!("dogmaIM::GetAttributeTypes: {err}", err = crate::logging::chain(&e));
            index_rowset(HEADER, "attributeName", Vec::new())
        }
    }
}

const INVITEM_HEADER: &[&str] = &[
    "itemID", "typeID", "ownerID", "locationID", "flag", "contraband", "singleton", "quantity",
    "groupID", "categoryID", "customInfo",
];

const INFO_HEADER: &[&str] = &["itemID", "invItem", "attributes", "activeEffects", "time"];

const ITEM_SELECT: &str = r#"
    SELECT e."itemID", e."typeID", e."ownerID", e."locationID", e."flag", e."contraband",
           e."singleton", e."quantity", t."groupID", g."categoryID", e."customInfo"
      FROM entity e
      JOIN "invTypes"  t ON t."typeID"  = e."typeID"
      JOIN "invGroups" g ON g."groupID" = t."groupID"
     WHERE "#;

struct Item {
    item_id: i64,
    type_id: i64,
    owner_id: i64,
    location_id: i64,
    group_id: i32,
    category_id: i32,
    line: Vec<Value>,
}

async fn ship_info(ctx: &Ctx<'_>) -> Value {
    let (Some(ship_id), Some(char_id)) = (ctx.session.ship_id, ctx.session.char_id) else {
        return Value::Dict(Vec::new());
    };
    let sql = format!(
        r#"{ITEM_SELECT} e."itemID" = $1 OR e."itemID" = $2
            OR (e."locationID" = $1 AND e."flag" BETWEEN 11 AND 34)"#
    );
    info_dict(ctx, "ShipGetInfo", &sql, &[ship_id, char_id]).await
}

fn params(ids: &[i64]) -> Vec<i32> {
    ids.iter().map(|i| i32::try_from(*i).unwrap_or(0)).collect()
}

async fn char_info(ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::Dict(Vec::new()) };
    let sql = format!(
        r#"{ITEM_SELECT} e."itemID" = $1
            OR (e."locationID" = $1 AND e."flag" IN (7, 61, 88, 89))"#
    );
    info_dict(ctx, "CharGetInfo", &sql, &[char_id]).await
}

async fn item_info(ctx: &Ctx<'_>, key: Option<&Value>) -> Value {
    let Some(item_id) = key.and_then(|v| v.as_i64()) else {
        if matches!(key, Some(Value::Tuple(_)) | Some(Value::List(_))) {
            warn!("ItemGetInfo for a sublocation {key:?}: not implemented");
        }
        return Value::None;
    };
    let sql = format!(r#"{ITEM_SELECT} e."itemID" = $1"#);
    let items = match load_items(ctx, "ItemGetInfo", &sql, &[item_id]).await {
        Some(v) => v,
        None => return Value::None,
    };
    let Some(item) = items.into_iter().next() else { return Value::None };
    let attrs = computed_attributes(ctx, &[item.item_id]).await;
    let now = crate::bulkdata::filetime_now();
    info_row(&item, &attrs, now)
}

async fn info_dict(ctx: &Ctx<'_>, what: &str, sql: &str, ids: &[i64]) -> Value {
    let Some(items) = load_items(ctx, what, sql, ids).await else {
        return Value::Dict(Vec::new());
    };
    let ids: Vec<i64> = items.iter().map(|i| i.item_id).collect();
    let attrs = computed_attributes(ctx, &ids).await;
    let now = crate::bulkdata::filetime_now();
    Value::Dict(
        items
            .iter()
            .map(|item| (Value::Int(item.item_id), info_row(item, &attrs, now)))
            .collect(),
    )
}

fn info_row(item: &Item, attrs: &BTreeMap<i64, BTreeMap<i32, f64>>, now: i64) -> Value {
    let own = attrs.get(&item.item_id);
    row(
        INFO_HEADER,
        vec![
            Value::Int(item.item_id),
            row(INVITEM_HEADER, item.line.clone()),
            attribute_dict(item, own),
            active_effects(item, own),
            Value::Long(now),
        ],
    )
}

async fn load_items(ctx: &Ctx<'_>, what: &str, sql: &str, ids: &[i64]) -> Option<Vec<Item>> {
    let vals = params(ids);
    let bound: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
        vals.iter().map(|v| v as &(dyn tokio_postgres::types::ToSql + Sync)).collect();
    let rows = match ctx.db.query(sql, &bound).await {
        Ok(r) => r,
        Err(e) => {
            error!("dogmaIM::{what}: {err}", err = crate::logging::chain(&e));
            return None;
        }
    };
    Some(
        rows.iter()
            .filter_map(|r| {
                let get = |i: usize| r.try_get::<_, i32>(i).ok().map(|v| v as i64);
                let item = Item {
                    item_id: get(0)?,
                    type_id: get(1)?,
                    owner_id: get(2).unwrap_or(0),
                    location_id: get(3).unwrap_or(0),
                    group_id: r.try_get::<_, i32>(8).unwrap_or(0),
                    category_id: r.try_get::<_, i32>(9).unwrap_or(0),
                    line: line(r),
                };
                let flag = r.try_get::<_, i32>(4).unwrap_or(0);
                if flag == 5 || flag == 87 {
                    return None;
                }
                if !ALLOWED_CATEGORIES.contains(&item.category_id)
                    && item.group_id != GROUP_CHARACTER
                {
                    return None;
                }
                let _ = item.type_id;
                Some(item)
            })
            .collect(),
    )
}

/// Attributes as the dogma engine computes them for the character's loadout
/// (character, skills, implants, ship and what is fitted); anything outside it
/// gets its stored values.
async fn computed_attributes(ctx: &Ctx<'_>, item_ids: &[i64]) -> BTreeMap<i64, BTreeMap<i32, f64>> {
    let mut out = BTreeMap::new();
    let mut rest = Vec::new();
    match ctx.session.char_id {
        Some(char_id) => {
            let active = std::collections::HashSet::new();
            match crate::dogma::Loadout::load(ctx.db, ctx.dogma, char_id, ctx.session.ship_id, &active).await {
                Ok(loadout) => {
                    for id in item_ids {
                        match loadout.find(*id) {
                            Some(i) => {
                                out.insert(*id, loadout.attributes(i).into_iter().collect());
                            }
                            None => rest.push(*id),
                        }
                    }
                }
                Err(e) => {
                    error!("dogma loadout: {}", crate::logging::chain(&e));
                    rest.extend_from_slice(item_ids);
                }
            }
        }
        None => rest.extend_from_slice(item_ids),
    }
    if !rest.is_empty() {
        out.extend(attributes_for(ctx, &rest).await);
    }
    live_vitals(ctx, &mut out);
    out
}

/// In space, the ship's hit points and capacitor are the fight's, not the
/// database's.
fn live_vitals(ctx: &Ctx<'_>, attrs: &mut BTreeMap<i64, BTreeMap<i32, f64>>) {
    use crate::combat::attr;
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else { return };
    let Some(own) = attrs.get_mut(&ship_id) else { return };
    let world = ctx.world.lock();
    let Some(fight) = world.parks.get(&system).and_then(|p| p.ships.get(&ship_id)).and_then(|s| s.fight.as_ref()) else {
        return;
    };
    let now = crate::combat::now_ms();
    own.insert(attr::SHIELD_CHARGE, fight.vitals.shield.at(now));
    own.insert(attr::CHARGE, fight.vitals.capacitor.at(now));
    own.insert(attr::ARMOR_DAMAGE, fight.vitals.armor_damage);
    own.insert(attr::DAMAGE, fight.vitals.hull_damage);
}

async fn attributes_for(
    ctx: &Ctx<'_>,
    item_ids: &[i64],
) -> BTreeMap<i64, BTreeMap<i32, f64>> {
    const NUMERIC: &str = r#"'^\s*-?[0-9]+(\.[0-9]+)?([eE][-+]?[0-9]+)?\s*$'"#;
    let sql = format!(
        r#"
        WITH scope AS (
            SELECT e."itemID", e."typeID" FROM entity e WHERE e."itemID" = ANY($1)
        ), merged AS (
            SELECT s."itemID", ta."attributeID", 1 AS arm,
                   COALESCE(ea."valueFloat", ea."valueInt"::float8,
                            CASE WHEN ta.value ~ {NUMERIC} THEN ta.value::float8 END) AS v
              FROM scope s
              JOIN "dgmTypeAttribs"     ta  ON ta."typeID"      = s."typeID"
              JOIN "dgmAttributeTypes"  dat ON dat."attributeID" = ta."attributeID"
              LEFT JOIN entity_attributes ea
                     ON ea."itemID" = s."itemID" AND ea."attributeID" = ta."attributeID"
            UNION ALL
            SELECT ea."itemID", ea."attributeID", 2 AS arm,
                   COALESCE(ea."valueFloat", ea."valueInt"::float8) AS v
              FROM entity_attributes ea
              JOIN scope s ON s."itemID" = ea."itemID"
              JOIN "dgmAttributeTypes" dat ON dat."attributeID" = ea."attributeID"
        )
        SELECT DISTINCT ON ("itemID", "attributeID") "itemID", "attributeID", v
          FROM merged WHERE v IS NOT NULL ORDER BY "itemID", "attributeID", arm"#
    );
    let ids: Vec<i32> = item_ids.iter().filter_map(|i| i32::try_from(*i).ok()).collect();
    let mut out: BTreeMap<i64, BTreeMap<i32, f64>> = BTreeMap::new();
    match ctx.db.query(sql.as_str(), &[&ids]).await {
        Ok(rows) => {
            for r in &rows {
                let (Ok(item), Ok(attr), Ok(v)) = (
                    r.try_get::<_, i32>(0),
                    r.try_get::<_, i32>(1),
                    r.try_get::<_, f64>(2),
                ) else {
                    continue;
                };
                out.entry(item as i64).or_default().insert(attr, v);
            }
        }
        Err(e) => error!("dogmaIM attributes: {err}", err = crate::logging::chain(&e)),
    }
    out
}

fn attribute_dict(item: &Item, own: Option<&BTreeMap<i32, f64>>) -> Value {
    let mut attrs: BTreeMap<i32, f64> = own.cloned().unwrap_or_default();
    for (value_attr, tau_attr, cap_attr) in CHARGED {
        if attrs.contains_key(&value_attr) {
            continue;
        }
        let (Some(&tau), Some(&cap)) = (attrs.get(&tau_attr), attrs.get(&cap_attr)) else {
            continue;
        };
        if tau != 0.0 && cap != 0.0 {
            attrs.insert(value_attr, cap);
        }
    }
    let _ = item;
    Value::Dict(
        attrs.into_iter().map(|(id, v)| (Value::Int(id as i64), number(v))).collect(),
    )
}

fn number(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        Value::Int(v as i64)
    } else {
        Value::Double(v)
    }
}

fn active_effects(item: &Item, own: Option<&BTreeMap<i32, f64>>) -> Value {
    let online = own.and_then(|a| a.get(&ATTR_IS_ONLINE)).copied().unwrap_or(0.0);
    if online == 0.0 {
        return Value::Dict(Vec::new());
    }
    let env = vec![
        Value::Int(item.item_id),
        Value::Int(item.owner_id),
        Value::Int(item.location_id),
        Value::None,
        Value::None,
        Value::List(Vec::new()),
        Value::Int(EFFECT_ONLINE),
    ];
    let mut d = env;
    d.push(Value::Long(crate::bulkdata::filetime_now()));
    d.push(Value::Int(0));
    d.push(Value::Int(0));
    d.push(Value::None);
    Value::Dict(vec![(Value::Int(EFFECT_ONLINE), Value::List(d))])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ship(attrs: &[(i32, f64)]) -> (Item, BTreeMap<i32, f64>) {
        let item = Item {
            item_id: 140_000_016,
            type_id: 606,
            owner_id: 140_000_006,
            location_id: 60_014_689,
            group_id: 237,
            category_id: 6,
            line: Vec::new(),
        };
        (item, attrs.iter().copied().collect())
    }

    fn get(v: &Value, key: i64) -> Option<&Value> {
        match v {
            Value::Dict(pairs) => {
                pairs.iter().find(|(k, _)| *k == Value::Int(key)).map(|(_, v)| v)
            }
            _ => None,
        }
    }

    #[test]
    fn synthesises_the_two_charged_attributes_at_full() {
        let (item, own) = ship(&[(55, 75000.0), (482, 100.0), (479, 500000.0), (263, 63.0)]);
        let d = attribute_dict(&item, Some(&own));
        assert_eq!(get(&d, 18), Some(&Value::Int(100)), "charge = capacitorCapacity");
        assert_eq!(get(&d, 264), Some(&Value::Int(63)), "shieldCharge = shieldCapacity");
    }

    #[test]
    fn an_override_beats_the_synthesised_full_value() {
        let (item, own) = ship(&[(55, 75000.0), (482, 100.0), (18, 42.5)]);
        let d = attribute_dict(&item, Some(&own));
        assert_eq!(get(&d, 18), Some(&Value::Double(42.5)));
    }

    #[test]
    fn no_charge_without_both_tau_and_cap() {
        let (item, own) = ship(&[(482, 100.0)]);
        assert_eq!(get(&attribute_dict(&item, Some(&own)), 18), None, "no tau");

        let (item, own) = ship(&[(55, 0.0), (482, 100.0)]);
        assert_eq!(get(&attribute_dict(&item, Some(&own)), 18), None, "zero tau");

        let (item, own) = ship(&[(55, 75000.0), (482, 0.0)]);
        assert_eq!(get(&attribute_dict(&item, Some(&own)), 18), None, "zero cap");
    }

    #[test]
    fn integral_values_travel_as_int() {
        assert_eq!(number(2.0), Value::Int(2));
        assert_eq!(number(-1.0), Value::Int(-1));
        assert_eq!(number(0.0), Value::Int(0));
        assert_eq!(number(2.5), Value::Double(2.5));
    }

    #[test]
    fn active_effects_follow_the_is_online_attribute() {
        let (item, own) = ship(&[(ATTR_IS_ONLINE, 0.0)]);
        assert_eq!(active_effects(&item, Some(&own)), Value::Dict(Vec::new()));
        assert_eq!(active_effects(&item, None), Value::Dict(Vec::new()));

        let (item, own) = ship(&[(ATTR_IS_ONLINE, 1.0)]);
        let d = active_effects(&item, Some(&own));
        let Some(Value::List(env)) = get(&d, EFFECT_ONLINE) else {
            panic!("online effect missing: {d:?}");
        };
        assert!(env.len() >= 11, "activeEffects value is {} long", env.len());
        assert_eq!(env[0], Value::Int(140_000_016), "self");
        assert_eq!(env[1], Value::Int(140_000_006), "char/owner");
        assert_eq!(env[2], Value::Int(60_014_689), "ship/location");
        assert_eq!(env[5], Value::List(Vec::new()), "area is a list");
        assert_eq!(env[6], Value::Int(EFFECT_ONLINE));
    }
}
