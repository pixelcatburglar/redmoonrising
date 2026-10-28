//! `petitioner`: the help window's petition box (petition.py), read side.
//!
//! Filing a petition needs a category, and no category data has been loaded
//! (petitionCategories is empty), so CreatePetition and the GM actions are not
//! served yet. What exists is shown and can be marked read.

use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::text;

const PETITION_HEADER: &[&str] = &[
    "petitionID", "petitionerID", "categoryID", "queueID", "subject", "petition", "createdAt",
    "claimed", "claimedBy", "closed", "closedAt", "escalatesTo",
];
const PETITION_SELECT: &str = r#"SELECT "petitionID","petitionerID","categoryID","queueID","subject",
                                        "petition","createdAt","claimed","claimedBy","closed",
                                        "closedAt","escalatesTo" FROM "petitions""#;

pub async fn petitioner(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let me = ctx.session.char_id.unwrap_or(0) as i32;
    let id = call.arg_id(0) as i32;
    let value = match call.method.as_str() {
        // CheckNewMessages reads .petitionID, .text and .messageID.
        "GetUnreadMessages" => {
            let sql = r#"SELECT m."messageID", m."petitionID", m."text" FROM "petitionMessages" m
                           JOIN "petitions" p ON p."petitionID" = m."petitionID"
                          WHERE p."petitionerID" = $1 AND m."isPetitionee" = 1 AND m."read" = 0
                          ORDER BY m."messageID""#;
            ctx.rowset(&["messageID", "petitionID", "text"], sql, &[&me]).await
        }
        "MarkAsRead" => {
            let sql = r#"UPDATE "petitionMessages" m SET "read" = 1 FROM "petitions" p
                          WHERE m."messageID" = $1 AND p."petitionID" = m."petitionID"
                            AND p."petitionerID" = $2"#;
            ctx.execute(sql, &[&id, &me]).await;
            Value::None
        }
        "GetMyPetitions" => {
            let sql = format!(
                r#"{PETITION_SELECT} WHERE "petitionerID" = $1 AND "deleted" = 0 ORDER BY "petitionID""#
            );
            ctx.rowset(PETITION_HEADER, &sql, &[&me]).await
        }
        "GetPetition" => {
            let sql = format!(r#"{PETITION_SELECT} WHERE "petitionID" = $1 AND "petitionerID" = $2"#);
            match ctx.query_one(&sql, &[&id, &me]).await {
                Some(r) => crate::svc::rows::row(PETITION_HEADER, crate::svc::rows::line(&r)),
                None => Value::None,
            }
        }
        "GetPetitionMessages" => {
            let sql = r#"SELECT m."messageID", m."petitionID", m."senderID", m."isPetitionee",
                                m."text", m."created", m."read"
                           FROM "petitionMessages" m
                           JOIN "petitions" p ON p."petitionID" = m."petitionID"
                          WHERE m."petitionID" = $1 AND p."petitionerID" = $2
                          ORDER BY m."messageID""#;
            let header =
                ["messageID", "petitionID", "senderID", "isPetitionee", "text", "created", "read"];
            ctx.rowset(&header, sql, &[&id, &me]).await
        }
        "GetPetitionLog" => {
            let sql = r#"SELECT l."logID", l."petitionID", l."logDateTime", l."actorID",
                                l."actionID", l."message"
                           FROM "petitionLog" l
                           JOIN "petitions" p ON p."petitionID" = l."petitionID"
                          WHERE l."petitionID" = $1 AND p."petitionerID" = $2
                          ORDER BY l."logID""#;
            let header = ["logID", "petitionID", "logDateTime", "actorID", "actionID", "message"];
            ctx.rowset(&header, sql, &[&id, &me]).await
        }
        "GetCategories" => {
            let sql = r#"SELECT "categoryID","displayName","description","escalatesTo"
                           FROM "petitionCategories" ORDER BY "categoryID""#;
            ctx.rowset(&["categoryID", "displayName", "description", "escalatesTo"], sql, &[]).await
        }
        "GetCategoryName" => {
            let sql = r#"SELECT "displayName" FROM "petitionCategories" WHERE "categoryID" = $1"#;
            let name = ctx.query_one(sql, &[&id]).await.and_then(|r| text(&r, 0));
            Value::text(name.unwrap_or_default())
        }
        "GetCategoryProperties" => {
            let sql = r#"SELECT "k" FROM "petitionCategoryProperties"
                          WHERE "categoryID" = $1 ORDER BY "orderID""#;
            ctx.rowset(&["k"], sql, &[&id]).await
        }
        "GetQueues" => {
            let sql = r#"SELECT "queueID","queueName" FROM "petitionQueues" ORDER BY "queueID""#;
            ctx.rowset(&["queueID", "queueName"], sql, &[]).await
        }
        "MayPetition" => Value::Int(1),
        _ => return None,
    };
    Some(Outcome::Value(value))
}
