//! `ramProxy`: the Science & Industry window (manufacturing.py). The listing side
//! only; installing and delivering jobs are not served yet.

use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::rowset;

/// manufacturing.py GetAssemblyLinesScrolllist reads these off each line, and
/// GetRegionDetails appends an activityID column itself.
const LINE_HEADER: &[&str] = &[
    "containerID", "containerTypeID", "containerLocationID", "assemblyLineTypeID", "quantity",
    "ownerID",
];

const JOB_HEADER: &[&str] = &[
    "jobID", "assemblyLineID", "installerID", "creator", "installedItemID", "installedItemTypeID",
    "installedItemOwnerID", "installedItemQuantity", "installedItemProductivityLevel",
    "installedItemMaterialLevel", "installedItemLicensedProductionRunsRemaining", "containerID",
    "containerTypeID", "containerLocationID", "installedInSolarSystemID", "activityID",
    "outputTypeID", "outputFlag", "outputLocationID", "runs", "licensedProductionRuns",
    "installTime", "beginProductionTime", "endProductionTime", "completed", "completedStatus",
    "charAddedCost",
];

pub async fn ram_proxy(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "AssemblyLinesSelect" => assembly_lines(ctx, call.arg_text(0).as_deref()).await,
        "GetJobs2" => jobs(ctx, call).await,
        _ => return None,
    };
    Some(Outcome::Value(value))
}

/// AssemblyLinesSelect(typeFlag): 'region' is every line in the caller's region;
/// 'char', 'corp' and 'alliance' are the lines that owner holds
/// (manufacturing.py:938-942).
async fn assembly_lines(ctx: &Ctx<'_>, type_flag: Option<&str>) -> Value {
    let s = ctx.session;
    let (column, id) = match type_flag {
        Some("char") => ("ownerID", s.char_id),
        Some("corp") => ("ownerID", s.corp_id),
        Some("alliance") => ("ownerID", s.alliance_id),
        _ => ("regionID", s.region_id),
    };
    let Some(id) = id else { return rowset(LINE_HEADER, Vec::new()) };
    let sql = format!(
        r#"SELECT "stationID","stationTypeID","solarSystemID","assemblyLineTypeID","quantity","ownerID"
             FROM "ramAssemblyLineStations" WHERE "{column}" = $1
            ORDER BY "stationID","assemblyLineTypeID""#
    );
    ctx.rowset(LINE_HEADER, &sql, &[&(id as i32)]).await
}

/// GetJobs2(ownerID, completed, fromDate, toDate): the jobs of the caller or its
/// corporation, delivered or not; the dates (FILETIME, or None) bound installTime.
async fn jobs(ctx: &Ctx<'_>, call: &Call) -> Value {
    let owner = call.arg_i64(0);
    let s = ctx.session;
    if owner.is_none() || (owner != s.char_id && owner != s.corp_id) {
        return rowset(JOB_HEADER, Vec::new());
    }
    let completed = i16::from(call.arg_i64(1).unwrap_or(0) != 0);
    let from = call.arg_i64(2);
    let to = call.arg_i64(3);
    let sql = format!(
        r#"SELECT {} FROM "ramJobs"
            WHERE "installedItemOwnerID" = $1 AND "completed" = $2
              AND ($3::bigint IS NULL OR "installTime" >= $3)
              AND ($4::bigint IS NULL OR "installTime" <= $4)
            ORDER BY "installTime" DESC"#,
        JOB_HEADER.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(",")
    );
    ctx.rowset(JOB_HEADER, &sql, &[&(owner.unwrap_or(0) as i32), &completed, &from, &to]).await
}
