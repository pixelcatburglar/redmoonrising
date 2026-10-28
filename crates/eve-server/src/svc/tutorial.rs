use eve_marshal::Value;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{key_val, lines, rowset};

pub async fn tutorial_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let arg = |i: usize| call.arg_i64(i);
    let value = match call.method.as_str() {
        "GetTutorials" => get_tutorials(ctx).await,
        "GetCriterias" => get_criterias(ctx).await,
        "GetTutorialInfo" => tutorial_info(ctx, arg(0).unwrap_or(0)).await,
        "GetTutorialVersionInfo" => tutorial_info(ctx, arg(0).unwrap_or(0)).await,

        "LogAppClosed" | "LogClosed" | "LogAborted" | "LogCompleted" => Value::None,

        _ => return None,
    };
    Some(Outcome::Value(value))
}

async fn get_tutorials(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["tutorialID", "tutorialVName", "nextTutorialID"];
    let sql = r#"SELECT "tutorialID","tutorialVName","nextTutorialID"
                   FROM tutorials ORDER BY "tutorialID""#;
    ctx.rowset(HEADER, sql, &[]).await
}

async fn get_criterias(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["criteriaID", "criteriaName", "messageText", "audioPath"];
    let sql = r#"SELECT "criteriaID","criteriaName","messageText","audioPath"
                   FROM tutorial_criteria ORDER BY "criteriaID""#;
    ctx.rowset(HEADER, sql, &[]).await
}

async fn tutorial_info(ctx: &Ctx<'_>, tutorial_id: i64) -> Value {
    const TUTORIAL_HEADER: &[&str] = &["tutorialID", "tutorialVName", "nextTutorialID"];
    const PAGE_HEADER: &[&str] =
        &["pageID", "pageNumber", "pageName", "text", "imagePath", "audioPath"];
    const CRITERIA_HEADER: &[&str] = &["criteriaID"];
    const PAGE_CRITERIA_HEADER: &[&str] = &["pageID", "criteriaID"];

    let id = tutorial_id as i32;

    let tutorial_sql = r#"SELECT "tutorialID","tutorialVName","nextTutorialID"
                            FROM tutorials WHERE "tutorialID" = $1"#;
    let tutorial_rows = match ctx.db.query(tutorial_sql, &[&id]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("GetTutorialInfo({tutorial_id}): {err}", err = crate::logging::chain(&e));
            return Value::None;
        }
    };
    if tutorial_rows.is_empty() {
        return Value::None;
    }

    let page_sql = r#"SELECT "pageID","pageNumber","pageName","text","imagePath","audioPath"
                        FROM tutorial_pages WHERE "tutorialID" = $1 ORDER BY "pageNumber""#;

    let criteria_sql = r#"SELECT "criteriaID" FROM tutorials_criterias
                           WHERE "tutorialID" = $1 ORDER BY "criteriaID""#;

    let page_criteria_sql = r#"SELECT pc."pageID", pc."criteriaID"
                                 FROM tutorial_page_criteria pc
                                 JOIN tutorial_pages p ON p."pageID" = pc."pageID"
                                WHERE p."tutorialID" = $1
                                ORDER BY p."pageNumber", pc."criteriaID""#;

    let pages = ctx.rowset(PAGE_HEADER, page_sql, &[&id]).await;
    let criterias = ctx.rowset(CRITERIA_HEADER, criteria_sql, &[&id]).await;
    let pagecriterias =
        ctx.rowset(PAGE_CRITERIA_HEADER, page_criteria_sql, &[&id]).await;

    key_val(vec![
        ("tutorial", rowset(TUTORIAL_HEADER, lines(&tutorial_rows))),
        ("pages", pages),
        ("criterias", criterias),
        ("pagecriterias", pagecriterias),
    ])
}
