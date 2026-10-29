use eve_marshal::Value;
use tracing::{error, info};

use crate::services::{Call, Ctx, Outcome};
use crate::svc::bound;
use crate::svc::inventory::{FLAG_CORP_MARKET, INVITEM_HEADER, INVITEM_SELECT};
use crate::svc::rows::{cell, index_rowset, int, line, lines, row, rowset};

pub async fn registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    Some(corp_registry(ctx, &call.method, &call.args).await)
}

pub async fn station_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let station = bound::parse(&call.service)
        .and_then(|b| b.params.first().copied())
        .or(ctx.session.station_id)
        .unwrap_or(0);
    Some(corp_station_mgr(ctx, &call.method, &call.args, Some(station)).await)
}

pub async fn corpmgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let arg0 = call.arg_id(0);
    let value = match call.method.as_str() {
        "GetCorporations" | "GetPublicInfo" => corporation_row(ctx, arg0).await,
        "GetCorporationIDForCharacter" => corporation_id_for_character(ctx, arg0).await,
        "GetCorporationWallet" => {
            return Some(Outcome::Unknown(
                "corpmgr::GetCorporationWallet needs a bound corp wallet object".into(),
            ))
        }
        "GetCorporationStations" => {
            let sql = r#"SELECT "stationID" AS "itemID", "solarSystemID" AS "locationID",
                                "stationTypeID" AS "typeID"
                           FROM "staStations" WHERE "corporationID" = $1 ORDER BY "stationID""#;
            let corp = ctx.session.corp_id.unwrap_or(0) as i32;
            ctx.rowset(&["itemID", "locationID", "typeID"], sql, &[&corp]).await
        }
        "GetItemsRented" => items_rented(ctx).await,
        // (corpID, which): the stations holding the corporation's assets of a kind.
        "GetAssetInventory" => {
            let which = call.arg_text(1).unwrap_or_default();
            match own_corp(ctx, arg0) {
                Some(corp) => asset_locations(ctx, corp, &which).await,
                None => rowset(&["locationID"], Vec::new()),
            }
        }
        // (corpID, locationID, which): those assets at one station.
        "GetAssetInventoryForLocation" => {
            let which = call.arg_text(2).unwrap_or_default();
            match own_corp(ctx, arg0) {
                Some(corp) => asset_items(ctx, corp, call.arg_id(1), &which).await,
                None => rowset(INVITEM_HEADER, Vec::new()),
            }
        }
        // (memberID, fromDate, toDate, rowsPerPage): item and role audit trails.
        // Neither is recorded yet.
        "AuditMember" => Value::Tuple(vec![
            rowset(&["eventDateTime", "eventTypeID", "actorID", "itemID", "typeID", "quantity", "locationID"], Vec::new()),
            rowset(&["changeTime", "characterID", "issuerID", "oldRoles", "newRoles", "grantable"], Vec::new()),
        ]),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

/// The corporation ID if it is the caller's own; assets are not public.
fn own_corp(ctx: &Ctx<'_>, corp_id: i64) -> Option<i32> {
    (ctx.session.corp_id == Some(corp_id)).then_some(corp_id as i32)
}

/// Office and other rentals (corp_ui_accounts.py reads typeID, rentedFromID,
/// startDate, rentPeriodInDays, flag).
async fn items_rented(ctx: &Ctx<'_>) -> Value {
    const DAY: i64 = 24 * 60 * 60 * 10_000_000;
    let sql = format!(
        r#"SELECT "typeID", "stationID" AS "rentedFromID",
                  "balanceDueDate"::bigint - "rentPeriodInDays"::bigint * {DAY} AS "startDate",
                  "rentPeriodInDays", "slotNumber" AS "flag"
             FROM "rentalInfo" WHERE "renterID" = $1 ORDER BY "stationID", "slotNumber""#
    );
    let corp = ctx.session.corp_id.unwrap_or(0) as i32;
    let header = ["typeID", "rentedFromID", "startDate", "rentPeriodInDays", "flag"];
    ctx.rowset(&header, &sql, &[&corp]).await
}

/// SQL restricting entity `e` to one kind of corporation asset; $1 is the corp.
fn asset_scope(which: &str) -> Option<String> {
    Some(match which {
        "offices" => r#"e."locationID" IN (SELECT "itemID" FROM "crpOffices" WHERE "corporationID" = $1)"#.into(),
        "property" => format!(
            r#"e."ownerID" = $1 AND e."flag" <> {FLAG_CORP_MARKET}
               AND e."locationID" IN (SELECT "stationID" FROM "staStations")"#
        ),
        "deliveries" => format!(r#"e."ownerID" = $1 AND e."flag" = {FLAG_CORP_MARKET}"#),
        _ => return None, // "junk", "jobs": nothing tracks these yet
    })
}

async fn asset_locations(ctx: &Ctx<'_>, corp: i32, which: &str) -> Value {
    let Some(scope) = asset_scope(which) else { return rowset(&["locationID"], Vec::new()) };
    // Office contents sit in the office item; report the station it is in.
    let sql = format!(
        r#"SELECT DISTINCT COALESCE(o."stationID", e."locationID") AS "locationID"
             FROM entity e
             LEFT JOIN "crpOffices" o ON o."itemID" = e."locationID" AND o."corporationID" = $1
            WHERE {scope} ORDER BY 1"#
    );
    ctx.rowset(&["locationID"], &sql, &[&corp]).await
}

async fn asset_items(ctx: &Ctx<'_>, corp: i32, station_id: i64, which: &str) -> Value {
    let Some(scope) = asset_scope(which) else { return rowset(INVITEM_HEADER, Vec::new()) };
    let sql = format!(
        r#"{INVITEM_SELECT} {scope}
             AND (e."locationID" = $2 OR e."locationID" IN
                  (SELECT "itemID" FROM "crpOffices" WHERE "corporationID" = $1 AND "stationID" = $2))
           ORDER BY e."itemID""#
    );
    ctx.rowset(INVITEM_HEADER, &sql, &[&corp, &(station_id as i32)]).await
}

pub async fn corporation_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let arg0 = call.arg_id(0);
    let value = match call.method.as_str() {
        "GetNPCDivisions" => npc_divisions(ctx).await,
        "GetEmploymentRecord" => employment_record(ctx, arg0).await,
        "GetCorpInfo" => corp_market_info(),
        "GetFactionInfo" => faction_info(ctx).await,
        _ => return None,
    };
    Some(Outcome::Value(value))
}

async fn corp_registry(ctx: &Ctx<'_>, method: &str, args: &[Value]) -> Outcome {
    let arg0 = args.first().and_then(|v| v.as_i64());
    let corp = ctx.session.corp_id.unwrap_or(0);

    let out = match method {
        "GetCorporation" => corporation_row(ctx, corp).await,
        "GetEveOwners" => eve_owners(ctx, corp).await,
        "GetMember" => member(ctx, arg0.unwrap_or(0)).await,
        "GetRoles" => {
            let roles = roles(ctx).await;
            rowset(ROLE_HEADER, roles.iter().map(role_line).collect())
        }
        "GetRoleGroups" => role_groups(ctx).await,
        "GetSharesByShareholder" => shares_by_shareholder(ctx, arg0.unwrap_or(0) != 0).await,
        "GetInfoWindowDataForChar" => info_window_data_for_char(ctx, arg0.unwrap_or(0)).await,
        "GetTitles" => index_rowset(TITLE_HEADER, "titleID", Vec::new()),
        // Items locked by a corporation vote: none, since there are no votes.
        "GetLockedItemLocations" => Value::List(Vec::new()),
        "MemberCanRunForCEO" => Value::Int(0),
        "GetMemberIDsWithMoreThanAvgShares" => Value::List(Vec::new()),

        "GetMembers" => {
            return Outcome::Unknown(
                "corpRegistry::GetMembers needs a bound util.SparseRowsetProvider".into(),
            )
        }

        _ => return Outcome::Unknown(format!("corpRegistry::{method}")),
    };
    Outcome::Value(out)
}

const CORP_HEADER: &[&str] = &[
    "corporationID", "corporationName", "description", "tickerName", "url", "taxRate",
    "minimumJoinStanding", "corporationType", "hasPlayerPersonnelManager",
    "sendCharTerminationMessage", "creatorID", "ceoID", "stationID", "raceID", "allianceID",
    "shares", "memberCount", "memberLimit", "allowedMemberRaceIDs", "graphicID", "shape1",
    "shape2", "shape3", "color1", "color2", "color3", "typeface", "division1", "division2",
    "division3", "division4", "division5", "division6", "division7", "balance", "deleted",
];

const CORP_SQL: &str = r#"SELECT "corporationID","corporationName","description","tickerName",
                                 "url","taxRate","minimumJoinStanding","corporationType",
                                 "hasPlayerPersonnelManager","sendCharTerminationMessage",
                                 "creatorID","ceoID","stationID","raceID","allianceID","shares",
                                 "memberCount","memberLimit","allowedMemberRaceIDs","graphicID",
                                 "shape1","shape2","shape3","color1","color2","color3","typeface",
                                 "division1","division2","division3","division4","division5",
                                 "division6","division7","balance","deleted"
                            FROM "corporation" WHERE "corporationID" = $1"#;

async fn corporation_row(ctx: &Ctx<'_>, corp_id: i64) -> Value {
    match ctx.db.query(CORP_SQL, &[&(corp_id as i32)]).await {
        Ok(rows) if !rows.is_empty() => row(CORP_HEADER, line(&rows[0])),
        Ok(_) => Value::None,
        Err(e) => {
            error!("GetCorporation({corp_id}): {err}", err = crate::logging::chain(&e));
            Value::None
        }
    }
}

async fn eve_owners(ctx: &Ctx<'_>, corp_id: i64) -> Value {
    const HEADER: &[&str] = &["ownerID", "ownerName", "typeID"];
    let sql = r#"SELECT "characterID","characterName","typeID"
                   FROM "character_" WHERE "corporationID" = $1"#;
    ctx.rowset(HEADER, sql, &[&(corp_id as i32)]).await
}

const MEMBER_HEADER: &[&str] = &[
    "characterID", "corporationID", "title", "startDateTime", "roles", "grantableRoles",
    "rolesAtHQ", "grantableRolesAtHQ", "rolesAtBase", "grantableRolesAtBase", "rolesAtOther",
    "grantableRolesAtOther", "baseID", "titleMask", "divisionID", "squadronID",
];

async fn member(ctx: &Ctx<'_>, char_id: i64) -> Value {
    let sql = r#"SELECT c."characterID", c."corporationID", c."title", c."corporationDateTime",
                        COALESCE(r."rolesAtAll", 0), COALESCE(r."rolesAtHQ", 0),
                        COALESCE(r."rolesAtBase", 0), COALESCE(r."rolesAtOther", 0)
                   FROM "character_" c
                   LEFT JOIN "chrCorporationRoles" r ON r."characterID" = c."characterID"
                  WHERE c."characterID" = $1"#;
    match ctx.db.query(sql, &[&(char_id as i32)]).await {
        Ok(rows) if !rows.is_empty() => {
            let r = &rows[0];
            let mask = |i: usize| Value::Long(r.get::<_, Option<i64>>(i).unwrap_or(0));
            row(
                MEMBER_HEADER,
                vec![
                    cell(r, 0),
                    cell(r, 1),
                    cell(r, 2),
                    cell(r, 3),
                    mask(4),
                    Value::Long(0),
                    mask(5),
                    Value::Long(0),
                    mask(6),
                    Value::Long(0),
                    mask(7),
                    Value::Long(0),
                    Value::None,
                    Value::Long(0),
                    Value::Int(1),
                    Value::Int(1),
                ],
            )
        }
        Ok(_) => Value::None,
        Err(e) => {
            error!("GetMember({char_id}): {err}", err = crate::logging::chain(&e));
            Value::None
        }
    }
}

const ROLE_HEADER: &[&str] = &["roleID", "roleName", "shortDescription", "description"];

struct CorpRole {
    id: i64,
    name: String,
    short: String,
    long: String,
}

fn role_line(r: &CorpRole) -> Value {
    Value::List(vec![
        Value::Long(r.id),
        Value::Bytes(r.name.clone().into_bytes()),
        Value::Bytes(r.short.clone().into_bytes()),
        Value::Bytes(r.long.clone().into_bytes()),
    ])
}

fn role_text(name: &str) -> Option<(String, String)> {
    let divisional = |prefix: &str| -> Option<u32> {
        let d = name.strip_prefix(prefix)?;
        match d.parse::<u32>() {
            Ok(n) if (1..=7).contains(&n) && d.len() == 1 => Some(n),
            _ => None,
        }
    };
    if let Some(n) = divisional("corpRoleHangarCanTake") {
        return Some((
            format!("Hangar Take Division {n}"),
            format!("May take items out of corporation hangar division {n}"),
        ));
    }
    if let Some(n) = divisional("corpRoleHangarCanQuery") {
        return Some((
            format!("Hangar Query Division {n}"),
            format!("May look at the contents of corporation hangar division {n}"),
        ));
    }
    if let Some(n) = divisional("corpRoleAccountCanTake") {
        return Some((
            format!("Account Take Division {n}"),
            format!("May withdraw funds from corporation account division {n}"),
        ));
    }
    let general = match name {
        "corpRoleDirector" => ("Director", "Has every role in the corporation"),
        "corpRolePersonnelManager" => {
            ("Personnel Manager", "May accept and reject applications to the corporation")
        }
        "corpRoleAccountant" => ("Accountant", "May view the corporation wallet and journal"),
        "corpRoleSecurityOfficer" => {
            ("Security Officer", "May set corporation standings and declare war")
        }
        "corpRoleFactoryManager" => {
            ("Factory Manager", "May rent and operate the corporation's factory slots")
        }
        "corpRoleStationManager" => {
            ("Station Manager", "May rent offices and manage the corporation's stations")
        }
        "corpRoleAuditor" => ("Auditor", "May read the corporation's member audit log"),
        _ => return None,
    };
    Some((general.0.to_string(), general.1.to_string()))
}

async fn roles(ctx: &Ctx<'_>) -> Vec<CorpRole> {
    let sql = r#"SELECT "constantID","constantValue" FROM "eveConstants"
                  WHERE "constantID" LIKE 'corpRole%' AND "constantValue" <> 4294967295
                  ORDER BY "constantValue""#;
    let rows = match ctx.db.query(sql, &[]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("GetRoles: {err}", err = crate::logging::chain(&e));
            Vec::new()
        }
    };
    let mut out = Vec::new();
    for r in &rows {
        let name: String = r.get(0);
        let id: i64 = r.get(1);
        if let Some((short, long)) = role_text(&name) {
            out.push(CorpRole { id, name, short, long });
        }
    }
    out
}

const ROLE_GROUP_HEADER: &[&str] =
    &["roleGroupID", "roleGroupName", "roleMask", "isDivisional", "appliesTo", "appliesToGrantable"];

async fn role_groups(ctx: &Ctx<'_>) -> Value {
    let roles = roles(ctx).await;
    let mask_of = |pred: &dyn Fn(&str) -> bool| -> i64 {
        roles.iter().filter(|r| pred(&r.name)).fold(0i64, |acc, r| acc | r.id)
    };
    let general = mask_of(&|n: &str| {
        !n.starts_with("corpRoleHangarCan") && !n.starts_with("corpRoleAccountCan")
    });
    let hangar = mask_of(&|n: &str| n.starts_with("corpRoleHangarCan"));
    let account = mask_of(&|n: &str| n.starts_with("corpRoleAccountCan"));

    let groups: &[(i64, &str, i64, i64, &str, &str)] = &[
        (0, "General Roles", general, 0, "roles", "grantableRoles"),
        (1, "Accounting Roles", account, 0, "roles", "grantableRoles"),
        (2, "Hangar Roles at HQ", hangar, 1, "rolesAtHQ", "grantableRolesAtHQ"),
        (4, "Hangar Roles at Base", hangar, 1, "rolesAtBase", "grantableRolesAtBase"),
        (6, "Hangar Roles at Other", hangar, 1, "rolesAtOther", "grantableRolesAtOther"),
    ];
    let lines = groups
        .iter()
        .map(|(id, name, mask, divisional, applies, grantable)| {
            Value::List(vec![
                Value::Int(*id),
                Value::Bytes(name.as_bytes().to_vec()),
                Value::Long(*mask),
                Value::Int(*divisional),
                Value::Bytes(applies.as_bytes().to_vec()),
                Value::Bytes(grantable.as_bytes().to_vec()),
            ])
        })
        .collect();
    rowset(ROLE_GROUP_HEADER, lines)
}

async fn shares_by_shareholder(ctx: &Ctx<'_>, corp_shares: bool) -> Value {
    const HEADER: &[&str] = &["corporationID", "shares"];
    if corp_shares {
        return rowset(HEADER, Vec::new());
    }
    let Some(char_id) = ctx.session.char_id else { return rowset(HEADER, Vec::new()) };
    let sql = r#"SELECT "corporationID","shares" FROM "crpCharShares" WHERE "characterID" = $1"#;
    ctx.rowset(HEADER, sql, &[&(char_id as i32)]).await
}

async fn info_window_data_for_char(ctx: &Ctx<'_>, char_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "corpID", "allianceID", "title", "title1", "title2", "title3", "title4", "title5",
        "title6", "title7", "title8",
    ];
    let sql = r#"SELECT "character_"."corporationID", "corporation"."allianceID",
                        "character_"."title"
                   FROM "character_"
                   JOIN "corporation"
                     ON "corporation"."corporationID" = "character_"."corporationID"
                  WHERE "character_"."characterID" = $1"#;
    match ctx.db.query(sql, &[&(char_id as i32)]).await {
        Ok(rows) if !rows.is_empty() => {
            let r = &rows[0];
            let mut line = line(r);
            line.resize(HEADER.len(), Value::None);
            row(HEADER, line)
        }
        Ok(_) => Value::None,
        Err(e) => {
            error!("GetInfoWindowDataForChar({char_id}): {err}", err = crate::logging::chain(&e));
            Value::None
        }
    }
}

async fn corporation_id_for_character(ctx: &Ctx<'_>, char_id: i64) -> Value {
    let sql = r#"SELECT "corporationID" FROM "character_" WHERE "characterID" = $1"#;
    match ctx.db.query(sql, &[&(char_id as i32)]).await {
        Ok(rows) if !rows.is_empty() => cell(&rows[0], 0),
        Ok(_) => Value::None,
        Err(e) => {
            error!("GetCorporationIDForCharacter({char_id}): {err}", err = crate::logging::chain(&e));
            Value::None
        }
    }
}

async fn npc_divisions(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["divisionID", "divisionName", "description", "leaderType"];
    let sql = r#"SELECT "divisionID","divisionName","description","leaderType"
                   FROM "crpNPCDivisions" ORDER BY "divisionID""#;
    ctx.rowset(HEADER, sql, &[]).await
}

async fn employment_record(ctx: &Ctx<'_>, char_id: i64) -> Value {
    const HEADER: &[&str] = &["corporationID", "startDate"];
    let sql = r#"SELECT "corporationID","startDate" FROM "chrEmployment"
                  WHERE "characterID" = $1 AND "deleted" = 0
                  ORDER BY "startDate" DESC"#;
    ctx.rowset(HEADER, sql, &[&(char_id as i32)]).await
}

fn corp_market_info() -> Value {
    const HEADER: &[&str] = &[
        "typeID", "sellPrice", "sellQuantity", "sellDateTime", "sellStationID", "buyPrice",
        "buyQuantity", "buyDateTime", "buyStationID",
    ];
    rowset(HEADER, Vec::new())
}

async fn faction_info(ctx: &Ctx<'_>) -> Value {
    let by_corp = int_pairs(
        ctx,
        r#"SELECT "corporationID","factionID" FROM "crpNPCCorporations"
            WHERE "factionID" IS NOT NULL"#,
    )
    .await;
    let regions = int_lists(
        ctx,
        r#"SELECT "factionID","regionID" FROM "mapRegions" WHERE "factionID" IS NOT NULL"#,
    )
    .await;
    let constellations = int_lists(
        ctx,
        r#"SELECT "factionID","constellationID" FROM "mapConstellations"
            WHERE "factionID" IS NOT NULL"#,
    )
    .await;
    let solar_systems = int_lists(
        ctx,
        r#"SELECT "factionID","solarSystemID" FROM "mapSolarSystems"
            WHERE "factionID" IS NOT NULL"#,
    )
    .await;
    let races = int_lists(ctx, r#"SELECT "factionID","raceID" FROM "factionRaces""#).await;
    let station_count = int_pairs(
        ctx,
        r#"SELECT "factionID","stationCount" FROM "chrFactions"
            WHERE "stationCount" IS NOT NULL"#,
    )
    .await;
    let system_count = int_pairs(
        ctx,
        r#"SELECT "factionID","stationSystemCount" FROM "chrFactions"
            WHERE "stationSystemCount" IS NOT NULL"#,
    )
    .await;

    const NPC_HEADER: &[&str] = &["corporationID", "size", "extent"];
    let sql = r#"SELECT "corporationID","size","extent" FROM "crpNPCCorporations""#;
    let npc_info = match ctx.db.query(sql, &[]).await {
        Ok(rows) => rows
            .iter()
            .map(|r| (cell(r, 0), row(NPC_HEADER, line(r))))
            .collect(),
        Err(e) => {
            error!("GetFactionInfo(npcCorpInfo): {err}", err = crate::logging::chain(&e));
            Vec::new()
        }
    };

    Value::Tuple(vec![
        Value::Dict(by_corp),
        Value::Dict(regions),
        Value::Dict(constellations),
        Value::Dict(solar_systems),
        Value::Dict(races),
        Value::Dict(station_count),
        Value::Dict(system_count),
        Value::Dict(npc_info),
    ])
}

async fn int_pairs(ctx: &Ctx<'_>, sql: &str) -> Vec<(Value, Value)> {
    match ctx.db.query(sql, &[]).await {
        Ok(rows) => rows
            .iter()
            .map(|r| {
                (
                    cell(r, 0),
                    cell(r, 1),
                )
            })
            .collect(),
        Err(e) => {
            error!("{sql}: {err}", err = crate::logging::chain(&e));
            Vec::new()
        }
    }
}

async fn int_lists(ctx: &Ctx<'_>, sql: &str) -> Vec<(Value, Value)> {
    let rows = match ctx.db.query(sql, &[]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("{sql}: {err}", err = crate::logging::chain(&e));
            return Vec::new();
        }
    };
    let mut keys: Vec<i64> = Vec::new();
    let mut vals: Vec<Vec<Value>> = Vec::new();
    for r in &rows {
        let k = r.get::<_, i32>(0) as i64;
        let v = cell(r, 1);
        match keys.iter().position(|x| *x == k) {
            Some(i) => vals[i].push(v),
            None => {
                keys.push(k);
                vals.push(vec![v]);
            }
        }
    }
    keys.into_iter().zip(vals).map(|(k, v)| (Value::Int(k), Value::List(v))).collect()
}

const TITLE_HEADER: &[&str] = &[
    "titleID", "titleName", "roles", "grantableRoles", "rolesAtHQ", "grantableRolesAtHQ",
    "rolesAtBase", "grantableRolesAtBase", "rolesAtOther", "grantableRolesAtOther",
];

async fn corp_station_mgr(
    ctx: &Ctx<'_>,
    method: &str,
    args: &[Value],
    station: Option<i64>,
) -> Outcome {
    let station = station.or(ctx.session.station_id).unwrap_or(0);
    let arg0 = args.first().and_then(|v| v.as_i64());

    let out = match method {
        "GetCorporateStationInfo" => corporate_station_info(ctx, station).await,

        "GetQuoteForRentingAnOffice" => office_rental_cost(ctx, station).await,

        "DoesPlayersCorpHaveJunkAtStation" => Value::Int(0),
        "GetQuoteForGettingCorpJunkBack" => Value::Double(0.0),

        "DoStandingCheckForStationService" => Value::None,

        "GetPotentialHomeStations" => potential_home_stations(ctx, station).await,

        "AreAnyFactorySlotsAvailableForRenting"
        | "AreAnyResearchFacilitySlotsAvailableForRenting" => Value::Int(0),
        "GetQuoteForRentingAFactorySlot" | "GetQuoteForRentingAResearchFacilitySlot" => {
            Value::Double(0.0)
        }

        "SetCloneTypeID" => return crate::svc::medical::set_clone_type(ctx, arg0.unwrap_or(0)).await,
        "SetHomeStation" => return crate::svc::medical::set_home_station(ctx, arg0.unwrap_or(0)).await,
        "RentOffice" => {
            let quoted = args.first().and_then(|v| v.as_f64()).unwrap_or(0.0);
            return rent_office(ctx, station, quoted).await;
        }

        _ => return Outcome::Unknown(format!("corpStationMgr::{method}")),
    };
    Outcome::Value(out)
}

async fn corporate_station_info(ctx: &Ctx<'_>, station: i64) -> Value {
    const OFFICE_HEADER: &[&str] = &["corporationID", "itemID", "officeFolderID"];

    let station = station as i32;

    let office_sql = r#"SELECT "corporationID","itemID","officeFolderID"
                          FROM "crpOffices" WHERE "stationID" = $1"#;
    let offices = ctx.rowset(OFFICE_HEADER, office_sql, &[&station]).await;

    let corp_sql = CORP_SQL.replace(
        r#"WHERE "corporationID" = $1"#,
        r#"WHERE "stationID" = $1
              OR "corporationID" IN (SELECT "corporationID" FROM "crpOffices"
                                      WHERE "stationID" = $1)"#,
    );
    let corp_rows = match ctx.db.query(corp_sql.as_str(), &[&station]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("GetCorporateStationInfo({station}) corporations: {err}", err = crate::logging::chain(&e));
            Vec::new()
        }
    };
    let corporations =
        rowset(CORP_HEADER, lines(&corp_rows));

    let mut ids: Vec<i32> = Vec::new();
    for r in &corp_rows {
        for i in [
            0usize,
            10,
            11,
        ] {
            if let Ok(Some(id)) = r.try_get::<_, Option<i32>>(i) {
                if id != 0 && !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    let eveowners = eve_owners_by_id(ctx, &ids).await;

    Value::Tuple(vec![eveowners, corporations, offices])
}

async fn eve_owners_by_id(ctx: &Ctx<'_>, ids: &[i32]) -> Value {
    const HEADER: &[&str] = &["ownerID", "ownerName", "typeID"];
    if ids.is_empty() {
        return rowset(HEADER, Vec::new());
    }
    const SQL: &str = r#"
        SELECT k.id AS "ownerID",
               COALESCE(o."ownerName", c."characterName", co."corporationName") AS "ownerName",
               COALESCE(NULLIF(o."typeID", 0), NULLIF(c."typeID", 0), bt."typeID",
                        CASE WHEN co."corporationID" IS NOT NULL THEN 2 END) AS "typeID"
          FROM unnest($1::int[]) AS k(id)
          LEFT JOIN "eveStaticOwners" o  ON o."ownerID"        = k.id
          LEFT JOIN "character_"      c  ON c."characterID"    = k.id
          LEFT JOIN "bloodlineTypes" bt  ON bt."bloodlineID"   = c."bloodlineID"
          LEFT JOIN "corporation"    co  ON co."corporationID" = k.id
         WHERE COALESCE(o."ownerName", c."characterName", co."corporationName") IS NOT NULL
           AND COALESCE(NULLIF(o."typeID", 0), NULLIF(c."typeID", 0), bt."typeID",
                        CASE WHEN co."corporationID" IS NOT NULL THEN 2 END) IS NOT NULL"#;
    ctx.rowset(HEADER, SQL, &[&ids]).await
}

async fn office_rental_cost(ctx: &Ctx<'_>, station: i64) -> Value {
    let sql = r#"SELECT "officeRentalCost"::double precision
                   FROM "staStations" WHERE "stationID" = $1"#;
    match ctx.db.query(sql, &[&(station as i32)]).await {
        Ok(rows) => match rows.first() {
            Some(r) => Value::Double(r.try_get::<_, Option<f64>>(0).ok().flatten().unwrap_or(0.0)),
            None => Value::Double(0.0),
        },
        Err(e) => {
            error!("GetQuoteForRentingAnOffice({station}): {err}", err = crate::logging::chain(&e));
            Value::Double(0.0)
        }
    }
}

/// cfg.IsNPC: IDs below this are NPC owners.
const FIRST_PLAYER_ID: i64 = 100_000_000;
const TYPE_OFFICE: i32 = 27;

/// RentOffice(cost): the caller's corporation takes an office here, paid from
/// the corp wallet. Every member online hears OnOfficeRentalChanged(corpID,
/// officeID, folderID) (bco_locations.py:200); the caller gets the officeID
/// (lobby.py:461).
async fn rent_office(ctx: &Ctx<'_>, station: i64, quoted: f64) -> Outcome {
    use crate::session::CORP_ROLE_DIRECTOR;
    use crate::svc::wallet::{move_cash, Payment, Transfer, REF_OFFICE_RENTAL_FEE};

    let Some(corp_id) = ctx.session.corp_id else { return Outcome::custom("You are not in a corporation.") };
    if corp_id < FIRST_PLAYER_ID {
        return Outcome::custom("NPC corporations cannot rent offices.");
    }
    if !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Outcome::custom("Only a director of your corporation can rent an office.");
    }
    let cost = office_rental_cost(ctx, station).await.as_f64().unwrap_or(0.0);
    if (cost - quoted).abs() > 0.5 {
        return Outcome::custom("The rent here has changed; please try again.");
    }

    // Claim the office first, so two directors cannot both pay for one.
    let claim = r#"
        WITH office AS (
            INSERT INTO "crpOffices" ("corporationID","stationID","itemID","typeID","officeFolderID")
            SELECT $1, $2, nextval('dyn_id_seq'), $3, nextval('office_folder_seq')
             WHERE NOT EXISTS (SELECT 1 FROM "crpOffices" WHERE "corporationID" = $1 AND "stationID" = $2)
            RETURNING "itemID","officeFolderID"
        ),
        item AS (
            INSERT INTO entity ("itemID","itemName","typeID","ownerID","locationID","flag",
                                "contraband","singleton","quantity","x","y","z","customInfo")
            SELECT "itemID", '', $3, $1, $2, 0, 0, 1, 1, 0, 0, 0, NULL FROM office
        )
        SELECT "itemID","officeFolderID" FROM office"#;
    let Some(r) = ctx.query_one(claim, &[&(corp_id as i32), &(station as i32), &TYPE_OFFICE]).await else {
        return Outcome::custom("Your corporation already has an office here.");
    };
    let office_id = int(&r, 0).unwrap_or(0);
    let folder_id = int(&r, 1).unwrap_or(0);

    let payment = Payment {
        payer: corp_id,
        payee: None,
        amount: cost,
        ref_type: REF_OFFICE_RENTAL_FEE,
        arg_id1: "",
        reason: "",
    };
    let refused = match move_cash(ctx.db_url, payment).await {
        Ok(()) => None,
        Err(Transfer::NotEnough { need, have }) => Some(Outcome::not_enough_money(need, have)),
        Err(Transfer::NoSuchOwner) => Some(Outcome::custom("Your corporation has no wallet.")),
        Err(Transfer::Db(e)) => {
            error!("office rent for {corp_id} at {station}: {}", crate::logging::chain(&e));
            Some(Outcome::custom("The rent could not be paid."))
        }
    };
    if let Some(refused) = refused {
        let release = r#"WITH office AS (DELETE FROM "crpOffices" WHERE "itemID" = $1)
                         DELETE FROM entity WHERE "itemID" = $1"#;
        ctx.execute(release, &[&(office_id as i32)]).await;
        return refused;
    }

    info!("corporation {corp_id} rented office {office_id} at station {station} for {cost:.0} ISK");
    let world = ctx.world.lock();
    let event = Value::Tuple(vec![Value::Int(corp_id), Value::Int(office_id), Value::Int(folder_id)]);
    world.notify(&world.corp_members(corp_id), "OnOfficeRentalChanged", event);
    Outcome::Value(Value::Int(office_id))
}

async fn potential_home_stations(ctx: &Ctx<'_>, station: i64) -> Value {
    const HEADER: &[&str] = &["stationID", "typeID"];
    let sql = r#"SELECT "stationID","stationTypeID" AS "typeID"
                   FROM "staStations" WHERE "stationID" = $1"#;
    ctx.rowset(HEADER, sql, &[&(station as i32)]).await
}
