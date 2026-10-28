use eve_marshal::Value;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::bound;
use crate::svc::rows::{index_rowset, line, lines, row, rowset};

const ALLIANCE: &str = "allianceRegistry";
const WAR: &str = "warRegistry";

const ALLIANCE_HEADER: &[&str] = &[
    "allianceID", "allianceName", "shortName", "description", "url", "executorCorpID",
    "creatorCorpID", "creatorCharID", "dictatorial", "startDate", "memberCount", "deleted",
];

const ALLIANCE_SELECT: &str = r#"
    SELECT "allianceID", "allianceName", "shortName", "description", "url", "executorCorpID",
           "creatorCorpID", "creatorCharID", "dictatorial", "startDate", "memberCount", "deleted"
      FROM alliances"#;

const WAR_HEADER: &[&str] = &[
    "warID", "declaredByID", "againstID", "timeDeclared", "timeFinished", "retracted", "mutual",
];

const WAR_SELECT: &str = r#"
    SELECT "warID", "declaredByID", "againstID", "timeDeclared", "timeFinished", "retracted",
           "mutual"
      FROM wars"#;

pub async fn alliance_registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let subject = bound::parse(&call.service).and_then(|b| b.params.first().copied());
    Some(alliance_method(ctx, &call.method, &call.args, subject).await)
}

pub async fn war_registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    Some(war_method(ctx, &call.method, &call.args).await)
}

async fn alliance_method(
    ctx: &Ctx<'_>,
    method: &str,
    args: &[Value],
    subject: Option<i64>,
) -> Outcome {
    match method {
        "GetAlliance" => {
            let id = args
                .first()
                .and_then(|v| v.as_i64())
                .or(subject)
                .or(ctx.session.alliance_id)
                .filter(|id| *id != 0);
            let Some(id) = id else {
                return Outcome::Unknown("allianceRegistry::GetAlliance with no alliance".into());
            };
            alliance_row(ctx, id).await
        }

        "GetRankedAlliances" => {
            let sql = format!(
                r#"{ALLIANCE_SELECT} WHERE "deleted" = 0 ORDER BY "memberCount" DESC, "allianceID""#
            );
            Outcome::Value(optional_rowset(ctx, "GetRankedAlliances", &sql, ALLIANCE_HEADER).await)
        }

        "GetMembers" => {
            Outcome::Value(index_rowset(MEMBER_HEADER, "corporationID", Vec::new()))
        }
        "GetApplications" => Outcome::Value(rowset(APPLICATION_HEADER, Vec::new())),
        "GetRelationships" => Outcome::Value(rowset(RELATIONSHIP_HEADER, Vec::new())),

        "GetBills" | "GetBillsReceivable" => Outcome::Value(rowset(BILL_HEADER, Vec::new())),
        "GetBillBalance" => Outcome::Value(Value::Double(0.0)),

        "UpdateAlliance" | "UpdateApplication" | "DeclareExecutorSupport" | "DeleteMember"
        | "SetRelationship" | "DeleteRelationship" | "RetractWar" | "DeclareWarAgainst"
        | "ChangeMutualWarFlag" | "PayBill" => Outcome::Unknown(format!(
            "{ALLIANCE}::{method} needs the alliances tables (db/migrations/0008_alliances.sql, \
             not yet applied)"
        )),

        _ => Outcome::Unknown(format!("{ALLIANCE}::{method}")),
    }
}

async fn alliance_row(ctx: &Ctx<'_>, alliance_id: i64) -> Outcome {
    let sql = format!(r#"{ALLIANCE_SELECT} WHERE "allianceID" = $1"#);
    let id32 = i32::try_from(alliance_id).unwrap_or(0);
    match ctx.db.query(sql.as_str(), &[&id32]).await {
        Ok(rows) if !rows.is_empty() => Outcome::Value(row(ALLIANCE_HEADER, line(&rows[0]))),
        Ok(_) => Outcome::Unknown(format!("no alliance {alliance_id}")),
        Err(e) if missing_table_is_empty(&e) => {
            Outcome::Unknown(format!("no alliance {alliance_id}: this database has no alliances table"))
        }
        Err(e) => {
            error!("{ALLIANCE}::GetAlliance({alliance_id}): {err}", err = crate::logging::chain(&e));
            Outcome::Unknown(format!("no alliance {alliance_id}"))
        }
    }
}

const MEMBER_HEADER: &[&str] =
    &["allianceID", "corporationID", "chosenExecutorID", "startDate"];
const APPLICATION_HEADER: &[&str] =
    &["allianceID", "corporationID", "applicationText", "applicationDateTime", "state"];
const RELATIONSHIP_HEADER: &[&str] = &["fromID", "toID", "relationship"];

const BILL_HEADER: &[&str] = &[
    "billID", "billTypeID", "debtorID", "creditorID", "amount", "dueDateTime", "interest",
    "externalID", "paid", "externalID2",
];

async fn war_method(ctx: &Ctx<'_>, method: &str, args: &[Value]) -> Outcome {
    match method {
        "GetWars" => {
            let owner = args
                .first()
                .and_then(|v| v.as_i64())
                .or(ctx.session.corp_id)
                .unwrap_or(0);
            let sql = format!(
                r#"{WAR_SELECT} WHERE "declaredByID" = $1 OR "againstID" = $1 ORDER BY "warID""#
            );
            Outcome::Value(optional_index_rowset(ctx, "GetWars", &sql, owner, WAR_HEADER).await)
        }
        "GetCostOfStartingAWarAgainst" => Outcome::Value(Value::Double(0.0)),
        _ => Outcome::Unknown(format!("{WAR}::{method}")),
    }
}

fn missing_table_is_empty(e: &tokio_postgres::Error) -> bool {
    e.code().map(|c| c.code()) == Some("42P01")
}

async fn optional_rowset(ctx: &Ctx<'_>, what: &str, sql: &str, header: &[&str]) -> Value {
    match ctx.db.query(sql, &[]).await {
        Ok(rows) => rowset(header, lines(&rows)),
        Err(e) => {
            if !missing_table_is_empty(&e) {
                error!("{what}: {err}", err = crate::logging::chain(&e));
            }
            rowset(header, Vec::new())
        }
    }
}

async fn optional_index_rowset(
    ctx: &Ctx<'_>,
    what: &str,
    sql: &str,
    owner_id: i64,
    header: &[&str],
) -> Value {
    let key = header.first().copied().unwrap_or("id");
    let id32 = i32::try_from(owner_id).unwrap_or(0);
    match ctx.db.query(sql, &[&id32]).await {
        Ok(rows) => index_rowset(
            header,
            key,
            rows.iter()
                .map(|r| {
                    let l = line(r);
                    (l.first().cloned().unwrap_or(Value::None), Value::List(l))
                })
                .collect(),
        ),
        Err(e) => {
            if !missing_table_is_empty(&e) {
                error!("{what}: {err}", err = crate::logging::chain(&e));
            }
            index_rowset(header, key, Vec::new())
        }
    }
}
