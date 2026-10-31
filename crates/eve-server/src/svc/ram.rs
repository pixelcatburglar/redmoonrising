//! `ramProxy`: the Science & Industry window (manufacturing.py): the assembly
//! lines on offer, installing jobs on them, and delivering what they make.
//!
//! A job runs on one assembly line of a station. It is quoted first
//! (InstallJob with quoteOnly=1) and installed once the pilot accepts the
//! price (quoteOnly=0, authorizedCost). The blueprint leaves the hangar for
//! the job (flagFactoryBlueprint) and comes back when the job is delivered or
//! cancelled; materials are taken at install and lost on cancelling.
//!
//! What a job needs is the blueprint's own rows in
//! TL2MaterialsForTypeWithActivity for the activity: skills (category 16, the
//! quantity being the level), materials used up whole (damagePerJob 1, with
//! waste when manufacturing), and tools, each run wearing out damagePerJob of
//! one.
//!
//! The rules, from the client where it shows them (infosvc.py:1064,
//! manufacturing.py GetSkillLimits) and from the game of the time where not:
//! - waste: wasteFactor / (1 + ME), or wasteFactor x (1 - ME) below 0; and
//!   Production Efficiency's 25% less 5% a level on top;
//! - manufacturing time: productionTime less PE / (1 + PE) of the
//!   productivityModifier, then 4% less a level of Industry;
//! - research: researchProductivityTime (PE) or researchMaterialTime (ME) a
//!   run, 5% less a level of Research or Metallurgy;
//! - copying: researchCopyTime x licensed runs / maxProductionLimit a copy,
//!   5% less a level of Science (my choice; the client says nothing);
//! - slots: 1 + Mass Production + Advanced Mass Production manufacturing jobs,
//!   1 + Laboratory Operation + Advanced Laboratory Operation others;
//! - reach: Supply Chain Management (manufacturing) and Scientific Networking
//!   (the rest): docked there, the system, 5, 10, 20 jumps, the region;
//! - price: the line's install cost and its cost per hour, by started hour,
//!   paid to the line's owner.
//!
//! Not served: jobs for a corporation, starbase arrays and laboratories,
//! invention (there are no activity 8 rows), standing discounts, and changing
//! a line's prices.

use eve_marshal::Value;
use tokio_postgres::GenericClient;
use tracing::info;

use crate::items::{Item, ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::svc::inventory::FLAG_FACTORY_BLUEPRINT;
use crate::svc::membership::{db_failed, message, Refused};
use crate::svc::rows::{float, int, key_val, row, rowset, text};
use crate::svc::wallet::{self, Entry, Transfer};

const ACTIVITY_MANUFACTURING: i64 = 1;
const ACTIVITY_TIME: i64 = 3;
const ACTIVITY_MATERIAL: i64 = 4;
const ACTIVITY_COPYING: i64 = 5;

const CATEGORY_BLUEPRINT: i64 = 9;
const CATEGORY_SKILL: i64 = 16;
const FLAG_HANGAR: i64 = 4;
const FLAG_CARGO: i64 = 5;

const STATUS_DELIVERED: i16 = 1;
const STATUS_ABORTED: i16 = 2;

const SKILL_INDUSTRY: i32 = 3380;
const SKILL_MASS_PRODUCTION: i32 = 3387;
const SKILL_PRODUCTION_EFFICIENCY: i32 = 3388;
const SKILL_SCIENCE: i32 = 3402;
const SKILL_RESEARCH: i32 = 3403;
const SKILL_LABORATORY_OPERATION: i32 = 3406;
const SKILL_METALLURGY: i32 = 3409;
const SKILL_SUPPLY_CHAIN_MANAGEMENT: i32 = 24268;
const SKILL_SCIENTIFIC_NETWORKING: i32 = 24270;
const SKILL_ADVANCED_LABORATORY_OPERATION: i32 = 24624;
const SKILL_ADVANCED_MASS_PRODUCTION: i32 = 24625;

/// The journal's refType for a job's fee: Manufacturing (56) to Copying (60).
const REF_JOB_BASE: i32 = 55;
/// const.ramMaxCopyRuns and ramMaxProductionTimeInDays.
const MAX_COPIES: i64 = 20;
const MAX_DAYS: f64 = 30.0;
/// Jumps of reach per level of Supply Chain Management or Scientific
/// Networking; -1 is the station one is docked in (GetSkillLimits).
const REACH: [i64; 6] = [-1, 0, 5, 10, 20, 50];

const SECOND: i64 = 10_000_000;

/// manufacturing.py GetAssemblyLinesScrolllist reads these off each line, and
/// GetRegionDetails appends an activityID column itself.
const LINE_HEADER: &[&str] = &[
    "containerID", "containerTypeID", "containerLocationID", "assemblyLineTypeID", "quantity",
    "ownerID",
];

/// One assembly line, as GetAssemblyLineDetailsScrolllist and
/// ShowAssemblyLineDetails read it.
const ASSEMBLY_LINE_HEADER: &[&str] = &[
    "assemblyLineID", "assemblyLineTypeID", "containerID", "nextFreeTime", "costInstall", "costPerHour",
    "restrictionMask", "discountPerGoodStandingPoint", "surchargePerBadStandingPoint", "minimumStanding",
    "minimumCharSecurity", "minimumCorpSecurity", "maximumCharSecurity", "maximumCorpSecurity", "ownerID",
    "activityID",
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

/// The quote window's fields (QuotePanel.LoadQuote): times in seconds, the
/// start as a FILETIME, and the bill of materials.
const QUOTE_HEADER: &[&str] = &[
    "maxJobStartTime", "productionTime", "cost", "installCost", "usageCost", "materialMultiplier",
    "charMaterialMultiplier", "timeMultiplier", "charTimeMultiplier", "bom",
];

pub async fn ram_proxy(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let result = match call.method.as_str() {
        "AssemblyLinesSelect" => Ok(assembly_lines(ctx, call.arg_text(0).as_deref()).await),
        "AssemblyLinesGet" => Ok(lines_at(ctx, call.arg_id(0)).await),
        "GetJobs2" => Ok(jobs(ctx, call).await),
        "InstallJob" => install(ctx, call).await,
        "CompleteJob" => complete(ctx, call.arg_id(1), call.arg_i64(2).unwrap_or(0) != 0).await,
        "UpdateAssemblyLineConfigurations" => Err(message("RamConfigAssemblyLinesAccessDenied")),
        _ => return None,
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
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

/// AssemblyLinesGet(containerID): the station's lines, each with when it is
/// next free.
async fn lines_at(ctx: &Ctx<'_>, container: i64) -> Value {
    let sql = format!(
        r#"SELECT l."assemblyLineID", l."assemblyLineTypeID", l."containerID", {NEXT_FREE},
                  l."costInstall", l."costPerHour", l."restrictionMask", l."discountPerGoodStandingPoint",
                  l."surchargePerBadStandingPoint", l."minimumStanding", l."minimumCharSecurity",
                  l."minimumCorpSecurity", l."maximumCharSecurity", l."maximumCorpSecurity", l."ownerID",
                  t."activityID"
             FROM "ramAssemblyLines" l JOIN "ramAssemblyLineTypes" t ON t."assemblyLineTypeID" = l."assemblyLineTypeID"
            WHERE l."containerID" = $1 ORDER BY l."assemblyLineID""#
    );
    ctx.rowset(ASSEMBLY_LINE_HEADER, &sql, &[&(container as i32)]).await
}

/// A line's nextFreeTime, a timestamp in UTC, as a FILETIME.
const NEXT_FREE: &str = r#"(EXTRACT(EPOCH FROM l."nextFreeTime") * 10000000)::bigint + 116444736000000000"#;

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

// ---- installing --------------------------------------------------------------------------

struct Line {
    id: i64,
    type_id: i64,
    station: i64,
    station_type: i64,
    system: i64,
    region: i64,
    owner: i64,
    activity: i64,
    time_multiplier: f64,
    material_multiplier: f64,
    cost_install: f64,
    cost_per_hour: f64,
    next_free: i64,
    min_security: f64,
    max_security: f64,
}

async fn line(db: &impl GenericClient, id: i64) -> Refused<Line> {
    let sql = format!(
        r#"SELECT l."assemblyLineID", l."assemblyLineTypeID", l."containerID", s."stationTypeID", s."solarSystemID",
                  s."regionID", l."ownerID", t."activityID", t."baseTimeMultiplier", t."baseMaterialMultiplier",
                  COALESCE(l."costInstall", 0), COALESCE(l."costPerHour", 0), {NEXT_FREE},
                  COALESCE(l."minimumCharSecurity", -10), COALESCE(l."maximumCharSecurity", 10)
             FROM "ramAssemblyLines" l
             JOIN "ramAssemblyLineTypes" t ON t."assemblyLineTypeID" = l."assemblyLineTypeID"
             JOIN "staStations" s ON s."stationID" = l."containerID"
            WHERE l."assemblyLineID" = $1"#
    );
    let r = db
        .query_opt(sql.as_str(), &[&(id as i32)])
        .await
        .map_err(|e| db_failed("reading the assembly line", e))?
        .ok_or_else(|| message("RamPleasePickAnInstalltion"))?;
    let i = |n| int(&r, n).unwrap_or(0);
    let f = |n| float(&r, n).unwrap_or(0.0);
    Ok(Line {
        id: i(0),
        type_id: i(1),
        station: i(2),
        station_type: i(3),
        system: i(4),
        region: i(5),
        owner: i(6),
        activity: i(7),
        time_multiplier: f(8),
        material_multiplier: f(9),
        cost_install: f(10),
        cost_per_hour: f(11),
        next_free: i(12),
        min_security: f(13),
        max_security: f(14),
    })
}

/// A blueprint item with its type's figures and its own research.
struct Blueprint {
    item: Item,
    type_name: String,
    product: i64,
    production_time: f64,
    research_productivity_time: f64,
    research_material_time: f64,
    research_copy_time: f64,
    productivity_modifier: f64,
    waste_factor: f64,
    max_production_limit: i64,
    copy: bool,
    material_level: i64,
    productivity_level: i64,
    runs_left: i64,
}

async fn blueprint(txn: &mut ItemTxn<'_>, item_id: i64) -> Refused<Blueprint> {
    let item = txn
        .item(item_id)
        .await
        .map_err(|e| db_failed("reading the blueprint", e))?
        .ok_or_else(|| message("RamPleasePickAnItemToInstall"))?;
    if item.category_id != CATEGORY_BLUEPRINT {
        return Err(message("RamActivityRequiresABlueprint"));
    }
    let sql = r#"SELECT t."typeName", b."productTypeID", b."productionTime", b."researchProductivityTime",
                        b."researchMaterialTime", b."researchCopyTime", b."productivityModifier", b."wasteFactor",
                        b."maxProductionLimit", COALESCE(i."copy", 0), COALESCE(i."materialLevel", 0),
                        COALESCE(i."productivityLevel", 0), COALESCE(i."licensedProductionRunsRemaining", -1)
                   FROM "invBlueprintTypes" b
                   JOIN "invTypes" t ON t."typeID" = b."blueprintTypeID"
                   LEFT JOIN "invBlueprints" i ON i."itemID" = $2
                  WHERE b."blueprintTypeID" = $1"#;
    let r = txn
        .tx()
        .query_opt(sql, &[&(item.type_id as i32), &(item_id as i32)])
        .await
        .map_err(|e| db_failed("reading the blueprint type", e))?
        .ok_or_else(|| message("RamActivityRequiresABlueprint"))?;
    let i = |n| int(&r, n).unwrap_or(0);
    let f = |n| float(&r, n).unwrap_or(0.0);
    Ok(Blueprint {
        type_name: text(&r, 0).unwrap_or_default(),
        product: i(1),
        production_time: f(2),
        research_productivity_time: f(3),
        research_material_time: f(4),
        research_copy_time: f(5),
        productivity_modifier: f(6),
        waste_factor: f(7),
        max_production_limit: i(8).max(1),
        copy: i(9) != 0,
        material_level: i(10),
        productivity_level: i(11),
        runs_left: i(12),
        item,
    })
}

/// One row of what a job needs.
#[derive(Debug, Clone, PartialEq)]
struct Need {
    type_id: i64,
    quantity: i64,
    damage: f64,
    skill: bool,
}

async fn needs(db: &impl GenericClient, blueprint_type: i64, activity: i64) -> Refused<Vec<Need>> {
    let sql = r#"SELECT m."requiredTypeID", m."quantity", m."damagePerJob", g."categoryID"
                   FROM "TL2MaterialsForTypeWithActivity" m
                   JOIN "invTypes" t ON t."typeID" = m."requiredTypeID"
                   JOIN "invGroups" g ON g."groupID" = t."groupID"
                  WHERE m."typeID" = $1 AND m."activity" = $2 AND m."quantity" > 0
                  ORDER BY m."requiredTypeID""#;
    let rows = db
        .query(sql, &[&(blueprint_type as i32), &(activity as i32)])
        .await
        .map_err(|e| db_failed("reading what the job needs", e))?;
    Ok(rows
        .iter()
        .map(|r| Need {
            type_id: int(r, 0).unwrap_or(0),
            quantity: int(r, 1).unwrap_or(0),
            damage: float(r, 2).unwrap_or(1.0),
            skill: int(r, 3) == Some(CATEGORY_SKILL),
        })
        .collect())
}

/// What a job will take, how long and what it costs.
#[derive(Debug, Clone, PartialEq)]
struct Plan {
    seconds: f64,
    begin: i64,
    end: i64,
    install_cost: f64,
    usage_cost: f64,
    material_multiplier: f64,
    char_material_multiplier: f64,
    time_multiplier: f64,
    char_time_multiplier: f64,
    /// (type, quantity) without waste, and the waste on top.
    raw: Vec<(i64, i64)>,
    waste: Vec<(i64, i64)>,
    tools: Vec<Need>,
    skills: Vec<(i64, i64)>,
    output_type: i64,
}

impl Plan {
    fn cost(&self) -> f64 {
        self.install_cost + self.usage_cost
    }

    /// Everything used up whole: the materials and their waste, summed.
    fn consumed(&self) -> Vec<(i64, i64)> {
        let mut total: Vec<(i64, i64)> = self.raw.clone();
        for (t, q) in &self.waste {
            match total.iter_mut().find(|(rt, _)| rt == t) {
                Some(e) => e.1 += q,
                None => total.push((*t, *q)),
            }
        }
        total
    }
}

/// The waste a blueprint's material level leaves (0.1 is 10%).
fn waste(waste_factor: f64, material_level: i64) -> f64 {
    let base = waste_factor / 100.0;
    if material_level >= 0 { base / (1.0 + material_level as f64) } else { base * (1.0 - material_level as f64) }
}

/// One manufacturing run's time at a productivity level (infosvc.py:1067).
fn run_time(production_time: f64, productivity_modifier: f64, productivity_level: i64) -> f64 {
    let pl = productivity_level as f64;
    if productivity_level >= 0 {
        production_time - pl / (1.0 + pl) * productivity_modifier
    } else {
        production_time - (pl - 1.0) * productivity_modifier
    }
}

/// The multipliers a line puts on what it makes: per group, else per
/// category; None if the line cannot make it.
async fn line_detail(db: &impl GenericClient, line_type: i64, subject: i64) -> Refused<Option<(f64, f64)>> {
    let sql = r#"SELECT COALESCE(
                    (SELECT ARRAY[d."timeMultiplier", d."materialMultiplier"] FROM "ramAssemblyLineTypeDetailPerGroup" d
                      WHERE d."assemblyLineTypeID" = $1 AND d."groupID" = t."groupID"),
                    (SELECT ARRAY[d."timeMultiplier", d."materialMultiplier"] FROM "ramAssemblyLineTypeDetailPerCategory" d
                      WHERE d."assemblyLineTypeID" = $1 AND d."categoryID" = g."categoryID"))
                   FROM "invTypes" t JOIN "invGroups" g ON g."groupID" = t."groupID"
                  WHERE t."typeID" = $2"#;
    let r = db.query_opt(sql, &[&(line_type as i32), &(subject as i32)]).await.map_err(|e| db_failed("reading the line's abilities", e))?;
    let m: Option<Vec<f64>> = r.and_then(|r| r.try_get(0).ok());
    Ok(m.filter(|m| m.len() == 2).map(|m| (m[0], m[1])))
}

/// InstallJob(installationLocationData, installedItemLocationData,
/// bomLocationData, flagOutput, runs, activityID, licensedProductionRuns,
/// forCorp, 'blah', quoteOnly=, authorizedCost=, installedItem=,
/// maxJobStartTime=) -> (quote, the blueprint's itemID if it was split off a
/// stack). The locations are [invLocation, path, ids] (manufacturing.py:1538).
async fn install(ctx: &Ctx<'_>, call: &Call) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let path_ids = |i: usize| call.args.get(i).and_then(|v| v.seq()).and_then(|p| p.get(2)).and_then(|v| v.seq()).map(|s| s.to_vec()).unwrap_or_default();
    let line_id = path_ids(0).get(1).and_then(|v| v.as_i64()).ok_or_else(|| message("RamPleasePickAnInstalltion"))?;
    let item_id = path_ids(1).first().and_then(|v| v.as_i64()).ok_or_else(|| message("RamPleasePickAnItemToInstall"))?;
    // bomLocationData: [invLocation, [[locationID, ownerID, flag]], []], or [[flagNone]] for none.
    let bom_flag = call
        .args
        .get(2)
        .and_then(|v| v.seq())
        .and_then(|b| b.get(1))
        .and_then(|v| v.seq())
        .and_then(|p| p.first())
        .and_then(|v| v.seq())
        .and_then(|step| if step.len() >= 3 { step.get(2) } else { None })
        .and_then(|v| v.as_i64());
    let output_flag = call.arg_i64(3).filter(|f| *f != 0).unwrap_or(FLAG_HANGAR);
    let runs = call.arg_i64(4).unwrap_or(1).max(1);
    let activity = call.arg_i64(5).unwrap_or(0);
    let licensed_runs = call.arg_i64(6).unwrap_or(0);
    let for_corp = call.args.get(7).is_some_and(|v| v.as_i64().unwrap_or(0) != 0 || *v == Value::Bool(true));
    let quote_only = call.kwarg("quoteOnly").and_then(|v| v.as_i64()).unwrap_or(1) != 0;
    let authorized = call.kwarg("authorizedCost").and_then(|v| v.as_f64());
    let latest_start = call.kwarg("maxJobStartTime").and_then(|v| v.as_i64());
    if for_corp {
        return Err(Outcome::custom("Installing jobs for a corporation is not supported yet."));
    }
    if ctx.session.ship_id.is_none() {
        return Err(message("RamNoShip"));
    }

    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
    let mut txn = ItemTxn::begin(&mut client).await.map_err(|e| db_failed("BEGIN", e))?;
    txn.tx()
        .execute(r#"SELECT 1 FROM "ramAssemblyLines" WHERE "assemblyLineID" = $1 FOR UPDATE"#, &[&(line_id as i32)])
        .await
        .map_err(|e| db_failed("locking the assembly line", e))?;
    let line = line(txn.tx(), line_id).await?;
    if line.activity != activity || ![ACTIVITY_MANUFACTURING, ACTIVITY_TIME, ACTIVITY_MATERIAL, ACTIVITY_COPYING].contains(&activity) {
        return Err(message("RamActivityInvalid"));
    }
    within_reach(ctx, &line).await?;
    let security = security_of(txn.tx(), me).await;
    if security < line.min_security {
        return Err(message("RamAccessDeniedSecStatusTooLow"));
    }
    if security > line.max_security {
        return Err(message("RamAccessDeniedSecStatusTooHigh"));
    }

    let mut bp = blueprint(&mut txn, item_id).await?;
    if bp.item.owner_id != me {
        return Err(message("RamCannotInstallItemForAnother"));
    }
    if !at_station(ctx, &mut txn, &bp.item, line.station).await? {
        return Err(message_with("RamInstalledItemWrongLocation", &[("location", Value::text(station_name(txn.tx(), line.station).await))]));
    }
    match activity {
        ACTIVITY_COPYING if bp.copy => return Err(message("RamCannotCopyABlueprintCopy")),
        ACTIVITY_TIME | ACTIVITY_MATERIAL if bp.copy => return Err(message("RamCannotResearchABlueprintCopy")),
        ACTIVITY_MANUFACTURING if bp.copy && runs > bp.runs_left => return Err(message("RamTooManyProductionRuns")),
        ACTIVITY_COPYING if runs > MAX_COPIES => return Err(Outcome::custom(format!("At most {MAX_COPIES} copies can be made in one job."))),
        _ => {}
    }
    if activity == ACTIVITY_MANUFACTURING && bp.product == 0 {
        return Err(message("RamNoKnownOutputType"));
    }
    let subject = if activity == ACTIVITY_MANUFACTURING { bp.product } else { bp.item.type_id };
    let (detail_time, detail_material) =
        line_detail(txn.tx(), line.type_id, subject).await?.ok_or_else(|| message("RamBadEndProductForActivity"))?;

    // A blueprint from a stack is installed alone.
    let mut split_off = Value::None;
    if bp.item.quantity > 1 {
        txn.set_quantity(bp.item.id, bp.item.quantity - 1).await.map_err(|e| db_failed("splitting the stack", e))?;
        let one = NewItem {
            type_id: bp.item.type_id,
            owner_id: bp.item.owner_id,
            location_id: bp.item.location_id,
            flag: bp.item.flag,
            quantity: 1,
            singleton: true,
        };
        let id = txn.create(one).await.map_err(|e| db_failed("splitting the stack", e))?;
        split_off = Value::Int(id);
        bp = blueprint(&mut txn, id).await?;
    }

    let need = needs(txn.tx(), bp.item.type_id, activity).await?;
    let skill_types: Vec<i32> = need
        .iter()
        .filter(|n| n.skill)
        .map(|n| n.type_id as i32)
        .chain([
            SKILL_INDUSTRY, SKILL_MASS_PRODUCTION, SKILL_PRODUCTION_EFFICIENCY, SKILL_SCIENCE, SKILL_RESEARCH,
            SKILL_LABORATORY_OPERATION, SKILL_METALLURGY, SKILL_ADVANCED_LABORATORY_OPERATION, SKILL_ADVANCED_MASS_PRODUCTION,
        ])
        .collect();
    let levels = crate::svc::skills::levels(txn.tx(), me, &skill_types).await.map_err(|e| db_failed("reading skills", e))?;
    let level = |s: i32| levels.get(&s).copied().unwrap_or(0);
    for n in need.iter().filter(|n| n.skill) {
        if level(n.type_id as i32) < n.quantity {
            let name = type_name(txn.tx(), n.type_id).await;
            return Err(message_with("RamNeedSkillForJob", &[("skillLevel", Value::Int(n.quantity)), ("item", Value::text(name))]));
        }
    }
    slots_free(txn.tx(), me, activity, &level).await?;

    let now = crate::bulkdata::filetime_now();
    let plan = plan(&line, &bp, &need, activity, runs, licensed_runs, (detail_time, detail_material), &level, now);
    if plan.seconds > MAX_DAYS * 86_400.0 {
        return Err(message_with("RamProductionTimeExceedsLimits", &[("productionTime", Value::Double(plan.seconds / 3600.0)), ("limit", Value::Double(MAX_DAYS * 24.0))]));
    }

    if quote_only {
        // The split-off blueprint is kept: the install refers to it.
        txn.commit().await.map_err(|e| db_failed("COMMIT", e))?;
        return Ok(Value::Tuple(vec![quote(&plan), split_off]));
    }
    if latest_start.is_some_and(|t| plan.begin > t) {
        return Err(message("RamCannotGuaranteeStartTime"));
    }
    if authorized.is_some_and(|a| plan.cost() > a + 0.01) {
        return Err(Outcome::custom("The price has gone up since your quote. Please ask for a new one."));
    }

    // The materials, from the hangar or the ship's hold at the station.
    let source = match bom_flag {
        Some(FLAG_CARGO) if ctx.session.station_id == Some(line.station) => ctx.session.ship_id.map(|s| (s, FLAG_CARGO)),
        Some(FLAG_HANGAR) => Some((line.station, FLAG_HANGAR)),
        _ => None,
    };
    let consumed = plan.consumed();
    if !consumed.is_empty() || !plan.tools.is_empty() {
        let Some((location, flag)) = source else { return Err(Outcome::custom("Please select an input location for the job.")) };
        let mut rng = crate::combat::Rng::seeded(now as u64 ^ (bp.item.id as u64) << 20);
        let mut take: Vec<(i64, i64, i64)> = consumed.iter().map(|(t, q)| (*t, *q, *q)).collect();
        for tool in &plan.tools {
            let worn = tool.damage * runs as f64;
            let used = worn.floor() as i64 + i64::from(rng.unit() < worn.fract());
            take.push((tool.type_id, tool.quantity.max(used), used));
        }
        for (t, have_at_least, used) in take {
            use_up(&mut txn, me, location, flag, t, have_at_least, used).await?;
        }
    }

    // The fee, from the pilot to the line's owner.
    let reason = format!("{} job, {} run(s)", activity_name(activity), runs);
    let entry = Entry { ref_type: REF_JOB_BASE + activity as i32, payer: me, payee: line.owner, arg_id1: "", reason: &reason };
    let balance = match wallet::post(txn.tx(), me, -plan.cost(), &entry).await {
        Ok(b) => b,
        Err(Transfer::NotEnough { need, have }) => return Err(Outcome::not_enough_money(need, have)),
        Err(Transfer::NoSuchOwner) => return Err(Outcome::custom("Your wallet could not be found.")),
        Err(Transfer::Db(e)) => return Err(db_failed("paying for the job", e)),
    };
    if plan.cost() > 0.0 {
        if let Err(Transfer::Db(e)) = wallet::post(txn.tx(), line.owner, plan.cost(), &entry).await {
            return Err(db_failed("paying the line's owner", e));
        }
    }

    txn.set_singleton(bp.item.id, true).await.map_err(|e| db_failed("installing the blueprint", e))?;
    txn.move_to(bp.item.id, line.station, i64::from(FLAG_FACTORY_BLUEPRINT)).await.map_err(|e| db_failed("installing the blueprint", e))?;
    let sql = r#"INSERT INTO "ramJobs" ("assemblyLineID", "installerID", "creator", "installedItemID", "installedItemTypeID",
                     "installedItemOwnerID", "installedItemQuantity", "installedItemProductivityLevel", "installedItemMaterialLevel",
                     "installedItemLicensedProductionRunsRemaining", "containerID", "containerTypeID", "containerLocationID",
                     "installedInSolarSystemID", "activityID", "outputTypeID", "outputFlag", "outputLocationID", "runs",
                     "licensedProductionRuns", "installTime", "beginProductionTime", "endProductionTime")
                 VALUES ($1, $2, $2, $3, $4, $2, 1, $5, $6, $7, $8, $9, $10, $10, $11, $12, $13, $8, $14, $15, $16, $17, $18)
                 RETURNING "jobID""#;
    let i = |v: i64| v as i32;
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 18] = [
        &i(line.id), &i(me), &i(bp.item.id), &i(bp.item.type_id), &i(bp.productivity_level), &i(bp.material_level),
        &i(bp.runs_left), &i(line.station), &i(line.station_type), &i(line.system), &i(activity), &i(plan.output_type),
        &i(output_flag), &i(runs), &i(licensed_runs), &now, &plan.begin, &plan.end,
    ];
    let job_id: i32 = txn.tx().query_one(sql, &params).await.map_err(|e| db_failed("recording the job", e))?.get(0);
    let free = r#"UPDATE "ramAssemblyLines" SET "nextFreeTime" = to_timestamp(($2 - 116444736000000000) / 1e7) AT TIME ZONE 'UTC'
                   WHERE "assemblyLineID" = $1"#;
    txn.tx().execute(free, &[&i(line.id), &plan.end]).await.map_err(|e| db_failed("booking the assembly line", e))?;
    let events = txn.commit().await.map_err(|e| db_failed("COMMIT", e))?;

    crate::items::notify(ctx.world, ctx.client_id, events);
    let w = ctx.world.lock();
    let cash = Value::Tuple(vec![Value::bytes("cash"), Value::Int(me), Value::Double(balance)]);
    w.notify(&w.clients_of(me), "OnAccountChange", cash);
    info!(
        "{me} installed job {job_id}: {} x{runs} of {} on line {} at {}, {:.0} s, {:.2} ISK",
        activity_name(activity), bp.type_name, line.id, line.station, plan.seconds, plan.cost()
    );
    Ok(Value::Tuple(vec![quote(&plan), split_off]))
}

/// The time, the price and the bill of materials of a job.
#[allow(clippy::too_many_arguments)]
fn plan(
    line: &Line,
    bp: &Blueprint,
    need: &[Need],
    activity: i64,
    runs: i64,
    licensed_runs: i64,
    (detail_time, detail_material): (f64, f64),
    level: &(dyn Fn(i32) -> i64 + Sync),
    now: i64,
) -> Plan {
    let line_time = line.time_multiplier * detail_time;
    let line_material = line.material_multiplier * detail_material;
    let less = |skill: i32, per_level: f64| 1.0 - per_level * level(skill) as f64;
    let (base_seconds, time_multiplier, char_time_multiplier) = match activity {
        ACTIVITY_MANUFACTURING => {
            let run = run_time(bp.production_time, bp.productivity_modifier, bp.productivity_level);
            (run, line_time * run / bp.production_time.max(1.0), less(SKILL_INDUSTRY, 0.04))
        }
        ACTIVITY_TIME => (bp.research_productivity_time, line_time, less(SKILL_RESEARCH, 0.05)),
        ACTIVITY_MATERIAL => (bp.research_material_time, line_time, less(SKILL_METALLURGY, 0.05)),
        _ => {
            let share = licensed_runs.clamp(1, bp.max_production_limit) as f64 / bp.max_production_limit as f64;
            (bp.research_copy_time * share, line_time, less(SKILL_SCIENCE, 0.05))
        }
    };
    let per_run = if activity == ACTIVITY_MANUFACTURING { bp.production_time } else { base_seconds };
    let seconds = per_run * time_multiplier * char_time_multiplier * runs as f64;

    let manufacturing = activity == ACTIVITY_MANUFACTURING;
    let material_multiplier = if manufacturing { line_material * (1.0 + waste(bp.waste_factor, bp.material_level)) } else { line_material };
    let char_material_multiplier = if manufacturing { 1.25 - 0.05 * level(SKILL_PRODUCTION_EFFICIENCY) as f64 } else { 1.0 };
    let (mut raw, mut waste_rows, mut tools, mut skills) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for n in need {
        if n.skill {
            skills.push((n.type_id, n.quantity));
        } else if n.damage >= 1.0 {
            let base = (n.quantity as f64 * line_material).round() as i64 * runs;
            let total = (n.quantity as f64 * material_multiplier * char_material_multiplier).round() as i64 * runs;
            raw.push((n.type_id, base));
            if total > base {
                waste_rows.push((n.type_id, total - base));
            }
        } else if n.damage > 0.0 {
            tools.push(n.clone());
        }
    }

    let begin = now.max(line.next_free);
    let hours = (seconds / 3600.0).ceil();
    Plan {
        seconds,
        begin,
        end: begin + (seconds * SECOND as f64) as i64,
        install_cost: line.cost_install,
        usage_cost: line.cost_per_hour * hours,
        material_multiplier,
        char_material_multiplier,
        time_multiplier,
        char_time_multiplier,
        raw,
        waste: waste_rows,
        tools,
        skills,
        output_type: if manufacturing { bp.product } else { bp.item.type_id },
    }
}

fn quote(p: &Plan) -> Value {
    let material = |(t, q): &(i64, i64)| key_val(vec![("requiredTypeID", Value::Int(*t)), ("quantity", Value::Int(*q))]);
    let extras = p
        .tools
        .iter()
        .map(|n| {
            key_val(vec![
                ("requiredTypeID", Value::Int(n.type_id)),
                ("quantity", Value::Int(n.quantity)),
                ("damagePerJob", Value::Double(n.damage)),
                ("isSkillCheck", Value::Int(0)),
            ])
        })
        .collect();
    let bom = key_val(vec![
        ("rawMaterials", Value::List(p.raw.iter().map(material).collect())),
        ("extras", Value::List(extras)),
        ("skills", Value::Dict(p.skills.iter().map(|(t, l)| (Value::Int(*t), Value::Int(*l))).collect())),
        ("wasteMaterials", Value::List(p.waste.iter().map(material).collect())),
    ]);
    row(
        QUOTE_HEADER,
        vec![
            Value::Long(p.begin),
            Value::Double(p.seconds),
            Value::Double(p.cost()),
            Value::Double(p.install_cost),
            Value::Double(p.usage_cost),
            Value::Double(p.material_multiplier),
            Value::Double(p.char_material_multiplier),
            Value::Double(p.time_multiplier),
            Value::Double(p.char_time_multiplier),
            bom,
        ],
    )
}

/// Whether the blueprint is in the station's hangar, or in the hold of a
/// ship there.
async fn at_station(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_>, item: &Item, station: i64) -> Refused<bool> {
    if item.location_id == station {
        return Ok(item.flag == FLAG_HANGAR);
    }
    if item.flag != FLAG_CARGO {
        return Ok(false);
    }
    let ship = txn.item(item.location_id).await.map_err(|e| db_failed("reading the ship", e))?;
    Ok(ship.is_some_and(|s| s.location_id == station && Some(s.owner_id) == ctx.session.char_id))
}

/// Takes `used` units of `t` from the stacks at a location (assembled ships
/// do not count), after checking `have_at_least` are there.
async fn use_up(txn: &mut ItemTxn<'_>, owner: i64, location: i64, flag: i64, t: i64, have_at_least: i64, mut used: i64) -> Refused<()> {
    let sql = r#"SELECT "itemID", "quantity" FROM entity
                  WHERE "locationID" = $1 AND "ownerID" = $2 AND "flag" = $3 AND "typeID" = $4 AND "singleton" = 0
                  ORDER BY "itemID" FOR UPDATE"#;
    let stacks: Vec<(i64, i64)> = txn
        .tx()
        .query(sql, &[&(location as i32), &(owner as i32), &(flag as i32), &(t as i32)])
        .await
        .map_err(|e| db_failed("counting the materials", e))?
        .iter()
        .map(|r| (int(r, 0).unwrap_or(0), int(r, 1).unwrap_or(0)))
        .collect();
    if stacks.iter().map(|(_, q)| q).sum::<i64>() < have_at_least {
        return Err(message_with("RamNeedMoreForJob", &[("item", Value::text(type_name(txn.tx(), t).await))]));
    }
    for (id, q) in stacks {
        if used <= 0 {
            break;
        }
        let take = q.min(used);
        txn.set_quantity(id, q - take).await.map_err(|e| db_failed("using the materials", e))?;
        used -= take;
    }
    Ok(())
}

/// Jobs a character may have running: manufacturing, or the research kinds
/// together.
async fn slots_free(db: &impl GenericClient, me: i64, activity: i64, level: &(dyn Fn(i32) -> i64 + Sync)) -> Refused<()> {
    let manufacturing = activity == ACTIVITY_MANUFACTURING;
    let allowed = if manufacturing {
        1 + level(SKILL_MASS_PRODUCTION) + level(SKILL_ADVANCED_MASS_PRODUCTION)
    } else {
        1 + level(SKILL_LABORATORY_OPERATION) + level(SKILL_ADVANCED_LABORATORY_OPERATION)
    };
    let sql = r#"SELECT count(*) FROM "ramJobs" WHERE "installerID" = $1 AND "completed" = 0
                    AND (("activityID" = 1) = $2)"#;
    let running = db
        .query_one(sql, &[&(me as i32), &manufacturing])
        .await
        .map_err(|e| db_failed("counting jobs", e))?
        .get::<_, i64>(0);
    if running >= allowed {
        let kind = if manufacturing { "manufacturing" } else { "research" };
        return Err(Outcome::custom(format!("Your skills allow {allowed} {kind} job(s) at a time, and you have {running} running.")));
    }
    Ok(())
}

/// Whether the caller may use a line where they are: their reach skill, in
/// jumps, from where they are.
async fn within_reach(ctx: &Ctx<'_>, line: &Line) -> Refused<()> {
    if ctx.session.station_id == Some(line.station) {
        return Ok(());
    }
    if ctx.session.region_id != Some(line.region) {
        return Err(message("RamRangeLimitationRegion"));
    }
    let skill = if line.activity == ACTIVITY_MANUFACTURING { SKILL_SUPPLY_CHAIN_MANAGEMENT } else { SKILL_SCIENTIFIC_NETWORKING };
    let me = ctx.session.char_id.unwrap_or(0);
    let level = crate::svc::skills::levels(ctx.db, me, &[skill]).await.map_err(|e| db_failed("reading skills", e))?;
    let reach = REACH[level.get(&skill).copied().unwrap_or(0).clamp(0, 5) as usize];
    let here = ctx.session.solar_system_id2.unwrap_or(0);
    let ok = match reach {
        -1 => false,
        50 => true,
        r => crate::gates::jumps(ctx.db, here, line.system, r).await.is_some(),
    };
    if ok {
        Ok(())
    } else if reach == -1 {
        Err(message_with("RamRangeLimitationJumpsNoSkill", &[("jumps", Value::text("some"))]))
    } else {
        Err(message_with("RamRangeLimitation", &[("range", Value::text(format!("jobs within {reach} jumps")))]))
    }
}

// ---- delivering --------------------------------------------------------------------------

/// CompleteJob(installationLocationData, jobID, cancel): a finished job's
/// output and its blueprint go to the owner's hangar at the station; a
/// cancelled one gives the blueprint back alone.
async fn complete(ctx: &Ctx<'_>, job_id: i64, cancel: bool) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
    let mut txn = ItemTxn::begin(&mut client).await.map_err(|e| db_failed("BEGIN", e))?;
    let sql = r#"SELECT "installedItemOwnerID", "installedItemID", "activityID", "outputTypeID", "outputLocationID",
                        "runs", "licensedProductionRuns", "endProductionTime", "completed"
                   FROM "ramJobs" WHERE "jobID" = $1 FOR UPDATE"#;
    let r = txn
        .tx()
        .query_opt(sql, &[&(job_id as i32)])
        .await
        .map_err(|e| db_failed("reading the job", e))?
        .ok_or_else(|| message("RamCompletionNoSuchJob"))?;
    let i = |n| int(&r, n).unwrap_or(0);
    let (owner, item_id, activity, output, station, runs, licensed_runs, end) = (i(0), i(1), i(2), i(3), i(4), i(5), i(6), i(7));
    if owner != me {
        return Err(message("RamCompletionAccessDenied"));
    }
    if i(8) != 0 {
        return Err(message("RamCompletionJobCompleted"));
    }
    if !cancel && crate::bulkdata::filetime_now() < end {
        return Err(message("RamCompletionInProduction"));
    }
    let bp = blueprint(&mut txn, item_id).await?;
    txn.move_to(item_id, station, FLAG_HANGAR).await.map_err(|e| db_failed("returning the blueprint", e))?;

    let research = |column: &str| {
        format!(
            r#"INSERT INTO "invBlueprints" ("itemID", "{column}") VALUES ($1, $2)
               ON CONFLICT ("itemID") DO UPDATE SET "{column}" = "invBlueprints"."{column}" + $2"#
        )
    };
    let delivered = async {
        if cancel {
            return Ok::<_, tokio_postgres::Error>(());
        }
        match activity {
            ACTIVITY_MANUFACTURING => {
                let portion = txn
                    .tx()
                    .query_opt(r#"SELECT "portionSize" FROM "invTypes" WHERE "typeID" = $1"#, &[&(output as i32)])
                    .await?
                    .and_then(|r| int(&r, 0))
                    .unwrap_or(1)
                    .max(1);
                let made = NewItem { type_id: output, owner_id: owner, location_id: station, flag: FLAG_HANGAR, quantity: portion * runs, singleton: false };
                txn.create(made).await?;
                if bp.copy {
                    let left = bp.runs_left - runs;
                    if left <= 0 {
                        txn.tx().execute(r#"DELETE FROM "invBlueprints" WHERE "itemID" = $1"#, &[&(item_id as i32)]).await?;
                        txn.destroy(item_id).await?;
                    } else {
                        let sql = r#"UPDATE "invBlueprints" SET "licensedProductionRunsRemaining" = $2 WHERE "itemID" = $1"#;
                        txn.tx().execute(sql, &[&(item_id as i32), &(left as i32)]).await?;
                    }
                }
            }
            ACTIVITY_TIME => {
                txn.tx().execute(research("productivityLevel").as_str(), &[&(item_id as i32), &(runs as i32)]).await?;
            }
            ACTIVITY_MATERIAL => {
                txn.tx().execute(research("materialLevel").as_str(), &[&(item_id as i32), &(runs as i32)]).await?;
            }
            _ => {
                for _ in 0..runs {
                    let copy = NewItem { type_id: output, owner_id: owner, location_id: station, flag: FLAG_HANGAR, quantity: 1, singleton: true };
                    let id = txn.create(copy).await?;
                    let sql = r#"INSERT INTO "invBlueprints" ("itemID", "copy", "materialLevel", "productivityLevel", "licensedProductionRunsRemaining")
                                 VALUES ($1, 1, $2, $3, $4)"#;
                    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 4] =
                        [&(id as i32), &(bp.material_level as i32), &(bp.productivity_level as i32), &(licensed_runs.max(1) as i32)];
                    txn.tx().execute(sql, &params).await?;
                }
            }
        }
        Ok(())
    }
    .await;
    delivered.map_err(|e| db_failed("delivering the job", e))?;
    let status = if cancel { STATUS_ABORTED } else { STATUS_DELIVERED };
    txn.tx()
        .execute(r#"UPDATE "ramJobs" SET "completed" = 1, "completedStatus" = $2 WHERE "jobID" = $1"#, &[&(job_id as i32), &status])
        .await
        .map_err(|e| db_failed("closing the job", e))?;
    let events = txn.commit().await.map_err(|e| db_failed("COMMIT", e))?;
    crate::items::notify(ctx.world, ctx.client_id, events);
    info!("{me} {} job {job_id} ({} of {})", if cancel { "cancelled" } else { "delivered" }, activity_name(activity), bp.type_name);
    Ok(Value::None)
}

// ---- shared ------------------------------------------------------------------------------

fn activity_name(activity: i64) -> &'static str {
    match activity {
        ACTIVITY_MANUFACTURING => "Manufacturing",
        ACTIVITY_TIME => "Time productivity research",
        ACTIVITY_MATERIAL => "Material research",
        ACTIVITY_COPYING => "Copying",
        _ => "Unknown",
    }
}

async fn security_of(db: &impl GenericClient, char_id: i64) -> f64 {
    let sql = r#"SELECT "securityRating" FROM "character_" WHERE "characterID" = $1"#;
    db.query_opt(sql, &[&(char_id as i32)]).await.ok().flatten().and_then(|r| float(&r, 0)).unwrap_or(0.0)
}

async fn type_name(db: &impl GenericClient, type_id: i64) -> String {
    let sql = r#"SELECT "typeName" FROM "invTypes" WHERE "typeID" = $1"#;
    db.query_opt(sql, &[&(type_id as i32)]).await.ok().flatten().and_then(|r| text(&r, 0)).unwrap_or_default()
}

async fn station_name(db: &impl GenericClient, station: i64) -> String {
    let sql = r#"SELECT "stationName" FROM "staStations" WHERE "stationID" = $1"#;
    db.query_opt(sql, &[&(station as i32)]).await.ok().flatten().and_then(|r| text(&r, 0)).unwrap_or_default()
}

fn message_with(key: &str, args: &[(&str, Value)]) -> Outcome {
    Outcome::UserError { msg: key.into(), args: args.iter().map(|(k, v)| ((*k).into(), v.clone())).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_research_shrinks_the_waste() {
        assert!((waste(10.0, 0) - 0.10).abs() < 1e-9);
        assert!((waste(10.0, 1) - 0.05).abs() < 1e-9);
        assert!((waste(10.0, 9) - 0.01).abs() < 1e-9);
        assert!((waste(10.0, -1) - 0.20).abs() < 1e-9);
    }

    #[test]
    fn productivity_research_shortens_a_run_as_the_info_window_shows() {
        assert_eq!(run_time(6000.0, 1200.0, 0), 6000.0);
        assert_eq!(run_time(6000.0, 1200.0, 1), 5400.0);
        assert_eq!(run_time(6000.0, 1200.0, -1), 8400.0);
    }

    fn rifter() -> (Line, Blueprint, Vec<Need>) {
        let line = Line {
            id: 1, type_id: 6, station: 60000004, station_type: 1531, system: 30000001, region: 10000001, owner: 1000010,
            activity: 1, time_multiplier: 1.0, material_multiplier: 1.0, cost_install: 1000.0, cost_per_hour: 333.0,
            next_free: 0, min_security: -10.0, max_security: 10.0,
        };
        let item = Item { id: 9, type_id: 691, owner_id: 1, location_id: 60000004, flag: 4, singleton: true, quantity: 1, category_id: 9, volume: 0.01 };
        let bp = Blueprint {
            item, type_name: "Rifter Blueprint".into(), product: 587, production_time: 6000.0, research_productivity_time: 120000.0,
            research_material_time: 120000.0, research_copy_time: 120000.0, productivity_modifier: 1200.0, waste_factor: 10.0,
            max_production_limit: 30, copy: false, material_level: 0, productivity_level: 0, runs_left: -1,
        };
        let need = vec![
            Need { type_id: 34, quantity: 20524, damage: 1.0, skill: false },
            Need { type_id: 3380, quantity: 1, damage: 0.0, skill: true },
            Need { type_id: 11483, quantity: 1, damage: 0.01, skill: false },
        ];
        (line, bp, need)
    }

    #[test]
    fn a_manufacturing_quote_adds_waste_and_charges_by_the_hour() {
        let (line, bp, need) = rifter();
        let no_skills = |_| 0;
        let p = plan(&line, &bp, &need, 1, 2, 0, (1.0, 1.0), &no_skills, 100);
        assert_eq!(p.raw, vec![(34, 41_048)]);
        // 10% from the blueprint, then 25% for no Production Efficiency.
        assert_eq!(p.waste, vec![(34, (20524.0f64 * 1.1 * 1.25).round() as i64 * 2 - 41_048)]);
        assert_eq!(p.skills, vec![(3380, 1)]);
        assert_eq!(p.tools.len(), 1);
        assert_eq!(p.seconds, 12_000.0);
        assert_eq!(p.usage_cost, 333.0 * 4.0);
        assert_eq!(p.end - p.begin, 12_000 * SECOND);
        assert_eq!(p.output_type, 587);
    }

    #[test]
    fn skills_cut_time_and_waste() {
        let (line, bp, need) = rifter();
        let level = |s| if s == SKILL_INDUSTRY || s == SKILL_PRODUCTION_EFFICIENCY { 5 } else { 0 };
        let p = plan(&line, &bp, &need, 1, 1, 0, (1.0, 1.0), &level, 0);
        assert!((p.seconds - 4800.0).abs() < 1e-6);
        assert!((p.char_material_multiplier - 1.0).abs() < 1e-9);
    }

    #[test]
    fn research_starts_when_the_line_is_free() {
        let (mut line, bp, need) = rifter();
        line.next_free = 1_000;
        let none = |_| 0;
        let p = plan(&line, &bp, &need, ACTIVITY_MATERIAL, 3, 0, (1.0, 1.0), &none, 10);
        assert_eq!(p.begin, 1_000);
        assert_eq!(p.seconds, 360_000.0);
        assert_eq!(p.output_type, 691);
        assert!(p.waste.is_empty());
    }
}
