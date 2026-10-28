//! `lookupSvc`: the name searches behind the people/places pickers (uix.py __Search).
//!
//! The client reads results by column name through uix.GetAttr (`characterID`,
//! `ownerName`, ...) and in places by position (`each[1]` is the ID), so every
//! result is `[<x>Name, <x>ID, typeID]`.

use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};

const TYPE_CORPORATION: i32 = 2;
const TYPE_FACTION: i32 = 30;
const TYPE_ALLIANCE: i32 = 16159;
/// cfg.IsNPC: IDs at or above this belong to players.
const FIRST_PLAYER_ID: i32 = 100_000_000;
const MAX_RESULTS: i64 = 500;

pub async fn lookup_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let search = call.arg_text(0).unwrap_or_default();
    let exact = call.arg_i64(1).unwrap_or(0) == 1;
    let value = match call.method.as_str() {
        "LookupOwners" => owners(ctx, &search, exact, false).await,
        "LookupPCOwners" | "LookupNoneNPCAccountOwners" => owners(ctx, &search, exact, true).await,
        "LookupCharacters" => {
            let sql = r#"SELECT "characterName", "characterID", "typeID" FROM "character_"
                          WHERE "accountID" IS NOT NULL AND {match}"#;
            find(ctx, &["characterName", "characterID", "typeID"], sql, "characterName", &search, exact).await
        }
        "LookupCorporations" => corporations(ctx, &search, exact, false).await,
        "LookupPlayerCorporations" => corporations(ctx, &search, exact, true).await,
        "LookupCorporationTickers" => {
            let sql = r#"SELECT "corporationName", "corporationID", 2 AS "typeID", "tickerName"
                           FROM "corporation" WHERE {match}"#;
            find(ctx, &["corporationName", "corporationID", "typeID", "tickerName"], sql, "tickerName", &search, exact).await
        }
        "LookupCorporationsOrAlliances" => {
            let sql = format!(
                r#"SELECT * FROM (
                       SELECT "corporationName" AS "ownerName", "corporationID" AS "ownerID",
                              {TYPE_CORPORATION} AS "typeID" FROM "corporation"
                       UNION ALL
                       SELECT "allianceName", "allianceID", {TYPE_ALLIANCE} FROM "alliances") o
                    WHERE {{match}}"#
            );
            find(ctx, &["ownerName", "ownerID", "typeID"], &sql, "ownerName", &search, exact).await
        }
        "LookupStations" => {
            let sql = r#"SELECT "stationName", "stationID", "stationTypeID" AS "typeID"
                           FROM "staStations" WHERE {match}"#;
            find(ctx, &["stationName", "stationID", "typeID"], sql, "stationName", &search, exact).await
        }
        "LookupFactions" => {
            let sql = format!(
                r#"SELECT "factionName", "factionID", {TYPE_FACTION} AS "typeID"
                     FROM "chrFactions" WHERE {{match}}"#
            );
            find(ctx, &["factionName", "factionID", "typeID"], &sql, "factionName", &search, exact).await
        }
        "LookupLocationsByGroup" => {
            // (groupID, searchStr): the arguments are the other way round here.
            let group = call.arg_i64(0).and_then(|g| i32::try_from(g).ok()).unwrap_or(0);
            let search = call.arg_text(1).unwrap_or_default();
            let sql = format!(
                r#"SELECT "itemName", "itemID", "typeID" FROM "mapDenormalize"
                    WHERE "groupID" = {group} AND {{match}}"#
            );
            find(ctx, &["itemName", "itemID", "typeID"], &sql, "itemName", &search, false).await
        }
        _ => return None,
    };
    Some(Outcome::Value(value))
}

async fn owners(ctx: &Ctx<'_>, search: &str, exact: bool, players_only: bool) -> Value {
    let filter = if players_only { format!(r#""ownerID" >= {FIRST_PLAYER_ID} AND "#) } else { String::new() };
    let sql = format!(
        r#"SELECT "ownerName", "ownerID", "typeID" FROM "eveStaticOwners" WHERE {filter}{{match}}"#
    );
    find(ctx, &["ownerName", "ownerID", "typeID"], &sql, "ownerName", search, exact).await
}

async fn corporations(ctx: &Ctx<'_>, search: &str, exact: bool, players_only: bool) -> Value {
    let filter = if players_only { format!(r#""corporationID" >= {FIRST_PLAYER_ID} AND "#) } else { String::new() };
    let sql = format!(
        r#"SELECT "corporationName", "corporationID", {TYPE_CORPORATION} AS "typeID"
             FROM "corporation" WHERE {filter}{{match}}"#
    );
    find(ctx, &["corporationName", "corporationID", "typeID"], &sql, "corporationName", search, exact).await
}

/// Runs `sql` with `{match}` replaced by a case-insensitive test of `column`
/// against `search`: equality when `exact`, otherwise "contains".
async fn find(
    ctx: &Ctx<'_>,
    header: &[&str],
    sql: &str,
    column: &str,
    search: &str,
    exact: bool,
) -> Value {
    if search.is_empty() {
        return crate::svc::rows::rowset(header, Vec::new());
    }
    let test = if exact {
        format!(r#"lower("{column}") = lower($1)"#)
    } else {
        format!(r#""{column}" ILIKE '%' || $1 || '%' ESCAPE '\'"#)
    };
    let sql = format!("{} ORDER BY 1 LIMIT {MAX_RESULTS}", sql.replace("{match}", &test));
    let pattern = if exact { search.to_string() } else { escape_like(search) };
    ctx.rowset(header, &sql, &[&pattern]).await
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::escape_like;

    #[test]
    fn like_wildcards_in_the_search_are_literal() {
        assert_eq!(escape_like("50%_off\\"), "50\\%\\_off\\\\");
    }
}
