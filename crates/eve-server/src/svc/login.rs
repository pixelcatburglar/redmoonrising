//! The calls a client makes between login and entering the game: cached bulk
//! objects, authentication housekeeping, and the character selection screen.
//! (`character::SelectCharacterID` itself changes the session, so `conn` serves it.)

use eve_marshal::Value;
use tracing::info;

use crate::services::{Call, Ctx, Outcome};

pub async fn object_caching(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if call.method != "GetCachableObject" {
        return None;
    }
    let object_id = call.arg_text(1).unwrap_or_default();
    Some(match ctx.bulk.entries.get(&object_id) {
        Some(entry) => Outcome::Value(ctx.bulk.payload(entry)),
        None => Outcome::Unknown(format!("no cached object {object_id:?}")),
    })
}

pub async fn authentication(call: &Call, _ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "Ping" => Value::Long(crate::bulkdata::filetime_now()),
        "GetPostAuthenticationMessage" | "SetLanguageID" => Value::None,
        _ => return None,
    };
    Some(Outcome::Value(value))
}

pub async fn character(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let char_id = call.arg_id(0);
    let value = match call.method.as_str() {
        "GetCharactersToSelect" => characters_to_select(ctx).await,
        "GetCharacterToSelect" => character_to_select(ctx, char_id).await,
        "PrepareCharacterForDelete" => prepare_for_delete(ctx, char_id).await,
        "CancelCharacterDeletePrepare" => {
            let sql = r#"UPDATE "character_" SET "deletePrepareDateTime" = NULL
                          WHERE "characterID" = $1 AND "accountID" = $2"#;
            ctx.execute(sql, &[&(char_id as i32), &account(ctx)]).await;
            Value::None
        }
        "DeleteCharacter" => delete_character(ctx, char_id).await,
        _ => return None,
    };
    Some(Outcome::Value(value))
}

const FILETIME_PER_SECOND: i64 = 10_000_000;

/// How long a character waits in the biomass queue before it can be deleted.
/// The client only displays it; the length is the server's choice.
const BIOMASS_DELAY: i64 = 60 * 60 * FILETIME_PER_SECOND;

fn account(ctx: &Ctx<'_>) -> i32 {
    ctx.session.user_id.unwrap_or(0) as i32
}

async fn characters_to_select(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &[
        "characterID", "characterName", "deletePrepareDateTime", "gender", "accessoryID",
        "beardID", "costumeID", "decoID", "eyebrowsID", "eyesID", "hairID", "lipstickID",
        "makeupID", "skinID", "backgroundID", "lightID", "headRotation1", "headRotation2",
        "headRotation3", "eyeRotation1", "eyeRotation2", "eyeRotation3", "camPos1", "camPos2",
        "camPos3", "morph1e", "morph1n", "morph1s", "morph1w", "morph2e", "morph2n", "morph2s",
        "morph2w", "morph3e", "morph3n", "morph3s", "morph3w", "morph4e", "morph4n", "morph4s",
        "morph4w",
    ];
    let sql = r#"SELECT "characterID","characterName","deletePrepareDateTime",
                        "gender","accessoryID","beardID","costumeID","decoID","eyebrowsID",
                        "eyesID","hairID","lipstickID","makeupID","skinID","backgroundID",
                        "lightID","headRotation1","headRotation2","headRotation3",
                        "eyeRotation1","eyeRotation2","eyeRotation3","camPos1","camPos2","camPos3",
                        "morph1e","morph1n","morph1s","morph1w","morph2e","morph2n","morph2s",
                        "morph2w","morph3e","morph3n","morph3s","morph3w","morph4e","morph4n",
                        "morph4s","morph4w"
                   FROM "character_" WHERE "accountID" = $1 ORDER BY "characterID""#;
    ctx.rowset(HEADER, sql, &[&account(ctx)]).await
}

async fn character_to_select(ctx: &Ctx<'_>, char_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "characterID", "petitionMessage", "gender", "bloodlineID", "stationID", "solarSystemID",
        "constellationID", "regionID", "startDateTime", "corporationID", "bounty",
        "securityRating", "title", "balance",
    ];
    let sql = r#"SELECT "characterID","petitionMessage","gender","bloodlineID","stationID",
                        "solarSystemID","constellationID","regionID","startDateTime",
                        "corporationID","bounty","securityRating","title","balance"
                   FROM "character_" WHERE "characterID" = $1 AND "accountID" = $2"#;
    ctx.rowset(HEADER, sql, &[&(char_id as i32), &account(ctx)]).await
}

/// Queues the character for deletion and returns when that becomes possible
/// (charsel.Terminate shows the wait), or None if it is not the caller's.
async fn prepare_for_delete(ctx: &Ctx<'_>, char_id: i64) -> Value {
    let when = crate::bulkdata::filetime_now() + BIOMASS_DELAY;
    let sql = r#"UPDATE "character_" SET "deletePrepareDateTime" = $3
                  WHERE "characterID" = $1 AND "accountID" = $2
              RETURNING "deletePrepareDateTime""#;
    match ctx.query_one(sql, &[&(char_id as i32), &account(ctx), &when]).await {
        Some(_) => {
            info!("character {char_id} queued for deletion");
            Value::Long(when)
        }
        None => Value::None,
    }
}

/// Deletes a queued character whose wait is over, with everything that belongs
/// only to it, in one statement. Shared history (kill logs, the market journal,
/// votes) stays. Returns None, or a (message, args) error the client shows.
async fn delete_character(ctx: &Ctx<'_>, char_id: i64) -> Value {
    let sql = r#"
        WITH target AS (
            SELECT "characterID" AS id, "corporationID" AS corp FROM "character_"
             WHERE "characterID" = $1 AND "accountID" = $2
               AND "deletePrepareDateTime" IS NOT NULL AND "deletePrepareDateTime" <= $3),
        items AS (
            DELETE FROM entity WHERE "ownerID" IN (SELECT id FROM target)
                                  OR "itemID"  IN (SELECT id FROM target)
            RETURNING "itemID"),
        attributes AS (DELETE FROM entity_attributes WHERE "itemID" IN (SELECT "itemID" FROM items)),
        owner      AS (DELETE FROM "eveStaticOwners"     WHERE "ownerID"     IN (SELECT id FROM target)),
        bookmarks  AS (DELETE FROM bookmarks             WHERE "ownerID"     IN (SELECT id FROM target)),
        notes      AS (DELETE FROM "chrNotes"            WHERE "characterID" IN (SELECT id FROM target)),
        ownernotes AS (DELETE FROM "chrOwnerNote"        WHERE "ownerID"     IN (SELECT id FROM target)),
        standings  AS (DELETE FROM "chrStandings"        WHERE "characterID" IN (SELECT id FROM target)),
        npcstand   AS (DELETE FROM "chrNPCStandings"     WHERE "characterID" IN (SELECT id FROM target)),
        roles      AS (DELETE FROM "chrCorporationRoles" WHERE "characterID" IN (SELECT id FROM target)),
        employment AS (DELETE FROM "chrEmployment"       WHERE "characterID" IN (SELECT id FROM target)),
        missions   AS (DELETE FROM "chrMissionState"     WHERE "characterID" IN (SELECT id FROM target)),
        offers     AS (DELETE FROM "chrOffers"           WHERE "characterID" IN (SELECT id FROM target)),
        loyalty    AS (DELETE FROM "chrAgentLoyalty"     WHERE "characterID" IN (SELECT id FROM target)),
        research   AS (DELETE FROM "chrAgentResearch"    WHERE "characterID" IN (SELECT id FROM target)),
        shares     AS (DELETE FROM "crpCharShares"       WHERE "characterID" IN (SELECT id FROM target)),
        applies    AS (DELETE FROM "crpApplications"     WHERE "characterID" IN (SELECT id FROM target)),
        orders     AS (DELETE FROM market_orders         WHERE "charID"      IN (SELECT id FROM target)),
        insurance  AS (DELETE FROM "insuranceContracts"  WHERE "ownerID"     IN (SELECT id FROM target)),
        channels   AS (DELETE FROM "channelChars"        WHERE "charID"      IN (SELECT id FROM target)),
        members    AS (UPDATE corporation SET "memberCount" = GREATEST("memberCount" - 1, 0)
                        WHERE "corporationID" IN (SELECT corp FROM target))
        DELETE FROM "character_" WHERE "characterID" IN (SELECT id FROM target)
        RETURNING "characterName""#;
    let now = crate::bulkdata::filetime_now();
    match ctx.query_one(sql, &[&(char_id as i32), &account(ctx), &now]).await {
        Some(_) => {
            info!("character {char_id} deleted");
            Value::None
        }
        None => Value::Tuple(vec![
            Value::bytes("CustomError"),
            Value::Dict(vec![(
                Value::bytes("error"),
                Value::bytes("That character is not on this account or is not ready for deletion."),
            )]),
        ]),
    }
}
