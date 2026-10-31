use std::sync::OnceLock;

use eve_marshal::Value;
use tracing::{debug, error, info, warn};

use crate::services::{Call, Ctx, Outcome};
use crate::svc::inventory::FLAG_HANGAR;
use crate::svc::rows::{cell, rowset};

pub async fn character(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetCharCreationInfo" => {
            cached(&CREATION_INFO, CREATION, ctx, "GetCharCreationInfo").await
        }
        "GetAppearanceInfo" => cached(&APPEARANCE_INFO, APPEARANCE, ctx, "GetAppearanceInfo").await,
        "ValidateName" => validate_name(call, ctx).await,
        "GetCharacterAppearanceList" => {
            let ids: Vec<i32> = call
                .args
                .first()
                .and_then(|v| v.seq())
                .map(|s| {
                    s.iter()
                        .filter_map(|v| v.as_i64())
                        .filter_map(|v| i32::try_from(v).ok())
                        .collect()
                })
                .unwrap_or_default();
            character_appearance_list(ctx, &ids).await
        }
        "CreateCharacter" => return Some(create_character(call, ctx).await),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

struct Catalogue {
    key: &'static str,
    sql: Option<&'static str>,
    header: &'static [&'static str],
}

const fn c(key: &'static str, header: &'static [&'static str], sql: &'static str) -> Catalogue {
    Catalogue { key, sql: Some(sql), header }
}

const fn empty(key: &'static str, header: &'static [&'static str]) -> Catalogue {
    Catalogue { key, sql: None, header }
}

const CREATION: &[Catalogue] = &[
    c(
        "races",
        &["raceID", "raceName", "description", "skillTypeID1", "typeID", "typeQuantity", "graphicID"],
        r#"SELECT "raceID","raceName","description","skillTypeID1","typeID","typeQuantity","graphicID"
             FROM "chrRaces" ORDER BY "raceID""#,
    ),
    c(
        "bloodlines",
        &[
            "bloodlineID", "bloodlineName", "raceID", "description", "maleDescription",
            "femaleDescription", "shipTypeID", "corporationID", "perception", "willpower",
            "charisma", "memory", "intelligence", "bonusTypeID", "skillTypeID1", "skillTypeID2",
            "graphicID",
        ],
        r#"SELECT "bloodlineID","bloodlineName","raceID","description","maleDescription",
                  "femaleDescription","shipTypeID","corporationID","perception","willpower",
                  "charisma","memory","intelligence","bonusTypeID","skillTypeID1","skillTypeID2",
                  "graphicID"
             FROM "chrBloodlines" ORDER BY "bloodlineID""#,
    ),
    c(
        "ancestries",
        &[
            "ancestryID", "ancestryName", "bloodlineID", "description", "postPerception",
            "postWillpower", "postCharisma", "postMemory", "postIntelligence", "perception",
            "willpower", "charisma", "memory", "intelligence", "skillTypeID1", "skillTypeID2",
            "typeID", "typeQuantity", "graphicID",
        ],
        r#"SELECT "ancestryID","ancestryName","bloodlineID","description",
                  "postPerception","postWillpower","postCharisma","postMemory","postIntelligence",
                  "perception","willpower","charisma","memory","intelligence",
                  "skillTypeID1","skillTypeID2","typeID","typeQuantity","graphicID"
             FROM "chrAncestries" ORDER BY "ancestryID""#,
    ),
    c(
        "attributes",
        &["attributeID", "attributeName", "description", "graphicID"],
        r#"SELECT "attributeID","attributeName","description","graphicID"
             FROM "chrAttributes" ORDER BY "attributeID""#,
    ),
    c(
        "schools",
        &[
            "raceID", "schoolID", "schoolName", "description", "graphicID", "corporationID",
            "agentID", "newAgentID",
        ],
        r#"SELECT "raceID","schoolID","schoolName","description","graphicID","corporationID",
                  "agentID","newAgentID"
             FROM "chrSchools" ORDER BY "schoolID""#,
    ),
    c(
        "departments",
        &[
            "schoolID", "departmentID", "departmentName", "description", "skillTypeID1",
            "skillTypeID2", "skillTypeID3", "graphicID",
        ],
        r#"SELECT "schoolID","departmentID","departmentName","description",
                  "skillTypeID1","skillTypeID2","skillTypeID3","graphicID"
             FROM "chrDepartments" ORDER BY "departmentID""#,
    ),
    c(
        "fields",
        &[
            "departmentID", "fieldID", "fieldName", "description", "skillTypeID1", "skillTypeID2",
            "typeID", "typeQuantity", "graphicID",
        ],
        r#"SELECT "departmentID","fieldID","fieldName","description",
                  "skillTypeID1","skillTypeID2","typeID","typeQuantity","graphicID"
             FROM "chrFields" ORDER BY "fieldID""#,
    ),
    c(
        "specialities",
        &[
            "fieldID", "specialityID", "specialityName", "description", "skillTypeID1",
            "skillTypeID2", "typeID", "typeQuantity", "graphicID",
        ],
        r#"SELECT "fieldID","specialityID","specialityName","description",
                  "skillTypeID1","skillTypeID2","typeID","typeQuantity","graphicID"
             FROM "chrSpecialities" ORDER BY "specialityID""#,
    ),
    c("skins", BL_HEADER_SKIN, r#"SELECT "bloodlineID","gender","skinID" FROM "chrBLSkins" WHERE npc = 0"#),
    c("eyes", BL_HEADER_EYES, r#"SELECT "bloodlineID","gender","eyesID" FROM "chrBLEyes" WHERE npc = 0"#),
    c("eyebrows", BL_HEADER_EYEBROWS, r#"SELECT "bloodlineID","gender","eyebrowsID" FROM "chrBLEyebrows" WHERE npc = 0"#),
    c("makeups", BL_HEADER_MAKEUP, r#"SELECT "bloodlineID","gender","makeupID" FROM "chrBLMakeups" WHERE npc = 0"#),
    c("lipsticks", BL_HEADER_LIPSTICK, r#"SELECT "bloodlineID","gender","lipstickID" FROM "chrBLLipsticks" WHERE npc = 0"#),
    c("decos", BL_HEADER_DECO, r#"SELECT "bloodlineID","gender","decoID" FROM "chrBLDecos" WHERE npc = 0"#),
    c("hairs", BL_HEADER_HAIR, r#"SELECT "bloodlineID","gender","hairID" FROM "chrBLHairs" WHERE npc = 0"#),
    c("beards", BL_HEADER_BEARD, r#"SELECT "bloodlineID","gender","beardID" FROM "chrBLBeards" WHERE npc = 0"#),
    c("accessories", BL_HEADER_ACCESSORY, r#"SELECT "bloodlineID","gender","accessoryID" FROM "chrBLAccessories" WHERE npc = 0"#),
    c("costumes", BL_HEADER_COSTUME, r#"SELECT "bloodlineID","gender","costumeID" FROM "chrBLCostumes" WHERE npc = 0"#),
    c("lights", &["lightID", "lightName"], r#"SELECT "lightID","lightName" FROM "chrBLLights""#),
    c(
        "backgrounds",
        &["backgroundID", "backgroundName"],
        r#"SELECT "backgroundID","backgroundName" FROM "chrBLBackgrounds""#,
    ),
    empty("benefits", &["benefitID", "benefitName", "description"]),
    empty("handicaps", &["handicapID", "handicapName", "description"]),
];

const BL_HEADER_SKIN: &[&str] = &["bloodlineID", "gender", "skinID"];
const BL_HEADER_EYES: &[&str] = &["bloodlineID", "gender", "eyesID"];
const BL_HEADER_EYEBROWS: &[&str] = &["bloodlineID", "gender", "eyebrowsID"];
const BL_HEADER_MAKEUP: &[&str] = &["bloodlineID", "gender", "makeupID"];
const BL_HEADER_LIPSTICK: &[&str] = &["bloodlineID", "gender", "lipstickID"];
const BL_HEADER_DECO: &[&str] = &["bloodlineID", "gender", "decoID"];
const BL_HEADER_HAIR: &[&str] = &["bloodlineID", "gender", "hairID"];
const BL_HEADER_BEARD: &[&str] = &["bloodlineID", "gender", "beardID"];
const BL_HEADER_ACCESSORY: &[&str] = &["bloodlineID", "gender", "accessoryID"];
const BL_HEADER_COSTUME: &[&str] = &["bloodlineID", "gender", "costumeID"];

const APPEARANCE: &[Catalogue] = &[
    c("skins", &["skinID", "skinName"], r#"SELECT "skinID","skinName" FROM "chrSkins""#),
    c("eyes", &["eyesID", "eyesName"], r#"SELECT "eyesID","eyesName" FROM "chrEyes""#),
    c("eyebrows", &["eyebrowsID", "eyebrowsName"], r#"SELECT "eyebrowsID","eyebrowsName" FROM "chrEyebrows""#),
    c("makeups", &["makeupID", "makeupName"], r#"SELECT "makeupID","makeupName" FROM "chrMakeups""#),
    c("lipsticks", &["lipstickID", "lipstickName"], r#"SELECT "lipstickID","lipstickName" FROM "chrLipsticks""#),
    c("decos", &["decoID", "decoName"], r#"SELECT "decoID","decoName" FROM "chrDecos""#),
    c("hairs", &["hairID", "hairName"], r#"SELECT "hairID","hairName" FROM "chrHairs""#),
    c("beards", &["beardID", "beardName"], r#"SELECT "beardID","beardName" FROM "chrBeards""#),
    c("accessories", &["accessoryID", "accessoryName"], r#"SELECT "accessoryID","accessoryName" FROM "chrAccessories""#),
    c("costumes", &["costumeID", "costumeName"], r#"SELECT "costumeID","costumeName" FROM "chrCostumes""#),
    c("lights", &["lightID", "lightName"], r#"SELECT "lightID","lightName" FROM "chrLights""#),
    c("backgrounds", &["backgroundID", "backgroundName"], r#"SELECT "backgroundID","backgroundName" FROM "chrBackgrounds""#),
];

static CREATION_INFO: OnceLock<Value> = OnceLock::new();
static APPEARANCE_INFO: OnceLock<Value> = OnceLock::new();

async fn cached(
    cell: &'static OnceLock<Value>,
    specs: &'static [Catalogue],
    ctx: &Ctx<'_>,
    what: &str,
) -> Value {
    if let Some(v) = cell.get() {
        return v.clone();
    }
    let (built, complete) = build(specs, ctx, what).await;
    if complete {
        let _ = cell.set(built.clone());
    }
    built
}

async fn build(specs: &[Catalogue], ctx: &Ctx<'_>, what: &str) -> (Value, bool) {
    let mut pairs = Vec::with_capacity(specs.len());
    let mut total = 0usize;
    let mut complete = true;
    for spec in specs {
        let value = match spec.sql {
            None => rowset(spec.header, Vec::new()),
            Some(sql) => match ctx.db.query(sql, &[]).await {
                Ok(rows) => {
                    total += rows.len();
                    let lines = rows
                        .iter()
                        .map(|r| {
                            Value::List((0..spec.header.len()).map(|i| cell(r, i)).collect())
                        })
                        .collect();
                    rowset(spec.header, lines)
                }
                Err(e) => {
                    error!(
                        "{what}: {} failed, serving it empty and NOT caching: {err}",
                        spec.key, err = crate::logging::chain(&e)
                    );
                    complete = false;
                    rowset(spec.header, Vec::new())
                }
            },
        };
        pairs.push((Value::bytes(spec.key), value));
    }
    debug!("{what}: built {} catalogues, {total} rows", specs.len());
    (Value::Dict(pairs), complete)
}

async fn validate_name(call: &Call, ctx: &Ctx<'_>) -> Value {
    let Some(name) = call.args.first().and_then(as_text) else {
        warn!("ValidateName: no name argument");
        return Value::Int(0);
    };
    if !name_shape_ok(&name) {
        debug!("ValidateName({name:?}) -> rejected (shape)");
        return Value::Int(0);
    }
    let sql = r#"SELECT 1 FROM "character_" WHERE lower("characterName") = lower($1) LIMIT 1"#;
    match ctx.db.query_opt(sql, &[&name]).await {
        Ok(None) => {
            debug!("ValidateName({name:?}) -> ok");
            Value::Int(1)
        }
        Ok(Some(_)) => {
            debug!("ValidateName({name:?}) -> rejected (taken)");
            Value::Int(0)
        }
        Err(e) => {
            error!("ValidateName({name:?}): {err}", err = crate::logging::chain(&e));
            Value::Int(0)
        }
    }
}

pub(crate) fn name_shape_ok(name: &str) -> bool {
    name_shape_error(name).is_none()
}

fn name_shape_error(name: &str) -> Option<&'static str> {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() < 4 {
        return Some("CharNameInvalidMinLength");
    }
    if chars.len() > 24 {
        return Some("CharNameInvalidMaxLength");
    }
    if chars.iter().filter(|c| **c == ' ').count() > 1 {
        return Some("CharNameInvalidMaxSpaces");
    }
    if !chars.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '\'')) {
        return Some("CharNameInvalidSomeChar");
    }
    let (Some(first), Some(last)) = (chars.first(), chars.last()) else {
        return Some("CharNameInvalid");
    };
    if !first.is_ascii_alphanumeric() {
        return Some("CharNameInvalidFirstChar");
    }
    if !last.is_ascii_alphanumeric() {
        return Some("CharNameInvalidLastChar");
    }
    None
}

fn as_text(v: &Value) -> Option<String> {
    match v {
        Value::Bytes(b) | Value::Buffer(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::Unicode(s) => Some(s.clone()),
        _ => None,
    }
}




enum Refusal {
    Message(&'static str),
    Custom(String),
}

impl Refusal {
    fn into_outcome(self) -> Outcome {
        match self {
            Refusal::Message(msg) => Outcome::UserError { msg: msg.into(), args: Vec::new() },
            Refusal::Custom(text) => Outcome::custom(text),
        }
    }
}

fn db_failed(what: &str, e: tokio_postgres::Error) -> Refusal {
    error!("CreateCharacter: {what}: {err}", err = crate::logging::chain(&e));
    Refusal::Custom("Character creation failed on the server. Please try again.".into())
}

async fn create_character(call: &Call, ctx: &Ctx<'_>) -> Outcome {
    let Some(account_id) = ctx.session.user_id.and_then(|v| i32::try_from(v).ok()) else {
        return Refusal::Custom("Not logged in.".into()).into_outcome();
    };

    let Some(name) = call.args.first().and_then(as_text) else {
        warn!("CreateCharacter: argument 1 (name) is missing or not a string");
        return Refusal::Message("CharNameInvalid").into_outcome();
    };
    let ids: Option<Vec<i32>> = (1..=12).map(|i| arg_i32(call, i)).collect();
    let Some(ids) = ids else {
        warn!("CreateCharacter({name:?}): arguments 2..13 are not all ints");
        return Refusal::Custom("Malformed character creation request.".into()).into_outcome();
    };
    let Some(Value::Dict(appearance)) = call.args.get(13) else {
        warn!("CreateCharacter({name:?}): argument 14 (appearance) is not a dict");
        return Refusal::Custom("Malformed character creation request.".into()).into_outcome();
    };

    let req = CreateRequest {
        name,
        bloodline_id: ids[0],
        gender: ids[1],
        ancestry_id: ids[2],
        school_id: ids[3],
        department_id: ids[4],
        field_id: ids[5],
        speciality_id: ids[6],
        spent: Spent {
            intelligence: ids[7],
            charisma: ids[8],
            perception: ids[9],
            memory: ids[10],
            willpower: ids[11],
        },
        appearance,
        account_id,
    };

    match insert_character(&req, ctx.db_url).await {
        Ok(char_id) => {
            info!("CreateCharacter({:?}) -> {char_id}", req.name);
            Outcome::Value(Value::Int(char_id as i64))
        }
        Err(refusal) => refusal.into_outcome(),
    }
}

struct CreateRequest<'a> {
    name: String,
    bloodline_id: i32,
    gender: i32,
    ancestry_id: i32,
    school_id: i32,
    department_id: i32,
    field_id: i32,
    speciality_id: i32,
    spent: Spent,
    appearance: &'a [(Value, Value)],
    account_id: i32,
}

struct Spent {
    intelligence: i32,
    charisma: i32,
    perception: i32,
    memory: i32,
    willpower: i32,
}

impl Spent {
    fn all(&self) -> [i32; 5] {
        [self.intelligence, self.charisma, self.perception, self.memory, self.willpower]
    }
}

const MAX_POINTS_PER_ATTRIBUTE: i32 = 3;
const POINTS_TO_ASSIGN: i32 = 5;
const MAX_ATTRIBUTE_VALUE: i32 = 20;
const MAX_CHARACTERS_PER_ACCOUNT: i64 = 3;

const FLAG_NONE: i32 = 0;
const FLAG_CARGO: i32 = 5;
const FLAG_SKILL: i32 = 7;

const DGM_CHARISMA: i32 = 164;
const DGM_INTELLIGENCE: i32 = 165;
const DGM_MEMORY: i32 = 166;
const DGM_PERCEPTION: i32 = 167;
const DGM_WILLPOWER: i32 = 168;
const DGM_SKILL_TIME_CONSTANT: i32 = 275;
const DGM_SKILL_POINTS: i32 = 276;
const DGM_SKILL_LEVEL: i32 = 280;

struct Slot {
    stem: &'static str,
    required: bool,
    sql: &'static str,
    keyed: bool,
}

const fn slot(stem: &'static str, required: bool, sql: &'static str) -> Slot {
    Slot { stem, required, sql, keyed: true }
}

const SLOTS: &[Slot] = &[
    slot("accessory", false, r#"SELECT 1 FROM "chrBLAccessories" WHERE "accessoryID"=$1 AND "bloodlineID"=$2 AND gender=$3 AND npc=0"#),
    slot("beard", false, r#"SELECT 1 FROM "chrBLBeards" WHERE "beardID"=$1 AND "bloodlineID"=$2 AND gender=$3 AND npc=0"#),
    slot("costume", true, r#"SELECT 1 FROM "chrBLCostumes" WHERE "costumeID"=$1 AND "bloodlineID"=$2 AND gender=$3 AND npc=0"#),
    slot("deco", false, r#"SELECT 1 FROM "chrBLDecos" WHERE "decoID"=$1 AND "bloodlineID"=$2 AND gender=$3 AND npc=0"#),
    slot("eyebrows", true, r#"SELECT 1 FROM "chrBLEyebrows" WHERE "eyebrowsID"=$1 AND "bloodlineID"=$2 AND gender=$3 AND npc=0"#),
    slot("eyes", true, r#"SELECT 1 FROM "chrBLEyes" WHERE "eyesID"=$1 AND "bloodlineID"=$2 AND gender=$3 AND npc=0"#),
    slot("hair", true, r#"SELECT 1 FROM "chrBLHairs" WHERE "hairID"=$1 AND "bloodlineID"=$2 AND gender=$3 AND npc=0"#),
    slot("lipstick", false, r#"SELECT 1 FROM "chrBLLipsticks" WHERE "lipstickID"=$1 AND "bloodlineID"=$2 AND gender=$3 AND npc=0"#),
    slot("makeup", false, r#"SELECT 1 FROM "chrBLMakeups" WHERE "makeupID"=$1 AND "bloodlineID"=$2 AND gender=$3 AND npc=0"#),
    slot("skin", true, r#"SELECT 1 FROM "chrBLSkins" WHERE "skinID"=$1 AND "bloodlineID"=$2 AND gender=$3 AND npc=0"#),
    Slot { stem: "background", required: true, keyed: false, sql: r#"SELECT 1 FROM "chrBLBackgrounds" WHERE "backgroundID"=$1"# },
    Slot { stem: "light", required: true, keyed: false, sql: r#"SELECT 1 FROM "chrBLLights" WHERE "lightID"=$1"# },
];

const ROTATIONS: &[&str] = &[
    "headRotation1", "headRotation2", "headRotation3",
    "eyeRotation1", "eyeRotation2", "eyeRotation3",
    "camPos1", "camPos2", "camPos3",
];

const MORPHS: &[&str] = &[
    "morph1e", "morph1n", "morph1s", "morph1w",
    "morph2e", "morph2n", "morph2s", "morph2w",
    "morph3e", "morph3n", "morph3s", "morph3w",
    "morph4e", "morph4n", "morph4s", "morph4w",
];

async fn insert_character(req: &CreateRequest<'_>, db_url: &str) -> Result<i32, Refusal> {
    let mut client = crate::db::connect(db_url)
        .await
        .map_err(|e| db_failed("cannot open a connection for the transaction", e))?;
    let tx = client
        .transaction()
        .await
        .map_err(|e| db_failed("BEGIN", e))?;

    let name = &req.name;
    if let Some(msg) = name_shape_error(name) {
        info!("CreateCharacter({name:?}) -> {msg}");
        return Err(Refusal::Message(msg));
    }

    let taken = tx
        .query_opt(
            r#"SELECT 1 FROM "character_" WHERE lower("characterName") = lower($1) LIMIT 1"#,
            &[name],
        )
        .await
        .map_err(|e| db_failed("name uniqueness check", e))?;
    if taken.is_some() {
        info!("CreateCharacter({name:?}) -> CharNameInvalidTaken");
        return Err(Refusal::Message("CharNameInvalidTaken"));
    }

    let used: i64 = tx
        .query_one(r#"SELECT count(*) FROM "character_" WHERE "accountID" = $1"#, &[&req.account_id])
        .await
        .map_err(|e| db_failed("character count", e))?
        .get(0);
    if used >= MAX_CHARACTERS_PER_ACCOUNT {
        info!("CreateCharacter({name:?}): account {} already has {used}", req.account_id);
        return Err(Refusal::Custom(format!(
            "This account already has {used} characters, which is the maximum."
        )));
    }

    let src = load_sources(&tx, req).await?;
    validate_points(&req.spent, &src)?;
    let appearance = read_appearance(&tx, req).await?;

    let now = crate::bulkdata::filetime_now();
    let char_id = next_id(&tx).await?;

    insert_entity(
        &tx,
        Entity {
            item_id: char_id,
            name,
            type_id: src.type_id,
            owner_id: char_id,
            location_id: src.station_id,
            flag: FLAG_NONE,
            singleton: 1,
            quantity: 1,
        },
    )
    .await?;

    insert_character_row(&tx, req, &src, &appearance, char_id, now).await?;

    tx.execute(
        r#"INSERT INTO "eveStaticOwners" ("ownerID","ownerName","typeID") VALUES ($1,$2,$3)"#,
        &[&char_id, &name.as_str(), &src.type_id],
    )
    .await
    .map_err(|e| db_failed("eveStaticOwners insert", e))?;
    tx.execute(
        r#"INSERT INTO "eveNames" ("itemID","itemName","categoryID","groupID","typeID")
           VALUES ($1,$2,1,1,$3)"#,
        &[&char_id, &name.as_str(), &src.type_id],
    )
    .await
    .map_err(|e| db_failed("eveNames insert", e))?;

    for (attribute_id, value) in [
        (DGM_INTELLIGENCE, src.base.intelligence + req.spent.intelligence + src.post.intelligence),
        (DGM_CHARISMA, src.base.charisma + req.spent.charisma + src.post.charisma),
        (DGM_PERCEPTION, src.base.perception + req.spent.perception + src.post.perception),
        (DGM_MEMORY, src.base.memory + req.spent.memory + src.post.memory),
        (DGM_WILLPOWER, src.base.willpower + req.spent.willpower + src.post.willpower),
    ] {
        insert_attribute(&tx, char_id, attribute_id, value).await?;
    }

    tx.execute(
        r#"INSERT INTO "chrCorporationRoles"
             ("characterID","corprole","rolesAtAll","rolesAtBase","rolesAtHQ","rolesAtOther")
           VALUES ($1,0,0,0,0,0)"#,
        &[&char_id],
    )
    .await
    .map_err(|e| db_failed("chrCorporationRoles insert", e))?;

    tx.execute(
        r#"INSERT INTO "chrEmployment" ("characterID","corporationID","startDate","deleted")
           VALUES ($1,$2,$3,0)"#,
        &[&char_id, &src.corporation_id, &now],
    )
    .await
    .map_err(|e| db_failed("chrEmployment insert", e))?;

    for (type_id, level) in &src.skills {
        let item_id = next_id(&tx).await?;
        insert_entity(
            &tx,
            Entity {
                item_id,
                name: "",
                type_id: *type_id,
                owner_id: char_id,
                location_id: char_id,
                flag: FLAG_SKILL,
                singleton: 1,
                quantity: 1,
            },
        )
        .await?;
        insert_attribute(&tx, item_id, DGM_SKILL_LEVEL, *level).await?;
        insert_attribute(&tx, item_id, DGM_SKILL_POINTS, skill_points(&tx, *type_id, *level).await?)
            .await?;
    }

    let ship_id = next_id(&tx).await?;
    insert_entity(
        &tx,
        Entity {
            item_id: ship_id,
            name: "",
            type_id: src.ship_type_id,
            owner_id: char_id,
            location_id: src.station_id,
            flag: FLAG_HANGAR,
            singleton: 1,
            quantity: 1,
        },
    )
    .await?;

    if let (Some(type_id), Some(quantity)) = (src.charge_type_id, src.charge_quantity) {
        let charge_id = next_id(&tx).await?;
        insert_entity(
            &tx,
            Entity {
                item_id: charge_id,
                name: "",
                type_id,
                owner_id: char_id,
                location_id: ship_id,
                flag: FLAG_CARGO,
                singleton: 0,
                quantity,
            },
        )
        .await?;
    }

    tx.execute(
        r#"UPDATE "character_" SET "shipID" = $2 WHERE "characterID" = $1"#,
        &[&char_id, &ship_id],
    )
    .await
    .map_err(|e| db_failed("character_.shipID update", e))?;

    for (type_id, quantity) in &src.equipment {
        let item_id = next_id(&tx).await?;
        insert_entity(
            &tx,
            Entity {
                item_id,
                name: "",
                type_id: *type_id,
                owner_id: char_id,
                location_id: src.station_id,
                flag: FLAG_HANGAR,
                singleton: 0,
                quantity: *quantity,
            },
        )
        .await?;
    }

    tx.commit().await.map_err(|e| db_failed("COMMIT", e))?;
    info!(
        "CreateCharacter({name:?}): id {char_id}, corp {}, station {}, {} skills, ship {}",
        src.corporation_id,
        src.station_id,
        src.skills.len(),
        src.ship_type_id
    );
    Ok(char_id)
}

async fn next_id(tx: &tokio_postgres::Transaction<'_>) -> Result<i32, Refusal> {
    let row = tx
        .query_one("SELECT nextval('dyn_id_seq')::integer", &[])
        .await
        .map_err(|e| db_failed("nextval(dyn_id_seq)", e))?;
    Ok(row.get(0))
}

struct Entity<'a> {
    item_id: i32,
    name: &'a str,
    type_id: i32,
    owner_id: i32,
    location_id: i32,
    flag: i32,
    singleton: i32,
    quantity: i32,
}

async fn insert_entity(tx: &tokio_postgres::Transaction<'_>, e: Entity<'_>) -> Result<(), Refusal> {
    tx.execute(
        r#"INSERT INTO entity
             ("itemID","itemName","typeID","ownerID","locationID","flag",
              "contraband","singleton","quantity","x","y","z","customInfo")
           VALUES ($1,$2,$3,$4,$5,$6,0,$7,$8,0,0,0,NULL)"#,
        &[
            &e.item_id, &e.name, &e.type_id, &e.owner_id, &e.location_id, &e.flag, &e.singleton,
            &e.quantity,
        ],
    )
    .await
    .map_err(|err| db_failed("entity insert", err))?;
    Ok(())
}

async fn insert_attribute(
    tx: &tokio_postgres::Transaction<'_>,
    item_id: i32,
    attribute_id: i32,
    value: i32,
) -> Result<(), Refusal> {
    tx.execute(
        r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt","valueFloat")
           VALUES ($1,$2,$3,NULL)"#,
        &[&item_id, &attribute_id, &i64::from(value)],
    )
    .await
    .map_err(|e| db_failed("entity_attributes insert", e))?;
    Ok(())
}

async fn skill_points(
    tx: &tokio_postgres::Transaction<'_>,
    type_id: i32,
    level: i32,
) -> Result<i32, Refusal> {
    if level <= 0 {
        return Ok(0);
    }
    let rank: Option<f64> = tx
        .query_opt(
            r#"SELECT COALESCE("valueInt", "valueFloat") FROM "dgmTypeAttributes"
                WHERE "typeID" = $1 AND "attributeID" = $2"#,
            &[&type_id, &DGM_SKILL_TIME_CONSTANT],
        )
        .await
        .map_err(|e| db_failed("skillTimeConstant lookup", e))?
        .map(|r| r.get(0));
    let rank = rank.unwrap_or_else(|| {
        warn!("CreateCharacter: skill type {type_id} has no skillTimeConstant, rank 1");
        1.0
    });
    Ok((rank * 250.0 * 2f64.powf(2.5 * (level as f64 - 1.0))).round() as i32)
}

#[derive(Default)]
struct Attrs {
    intelligence: i32,
    charisma: i32,
    perception: i32,
    memory: i32,
    willpower: i32,
}

struct Sources {
    type_id: i32,
    race_id: i32,
    ship_type_id: i32,
    corporation_id: i32,
    station_id: i32,
    solar_system_id: i32,
    constellation_id: i32,
    region_id: i32,
    base: Attrs,
    post: Attrs,
    charge_type_id: Option<i32>,
    charge_quantity: Option<i32>,
    skills: Vec<(i32, i32)>,
    equipment: Vec<(i32, i32)>,
}

async fn load_sources(
    tx: &tokio_postgres::Transaction<'_>,
    req: &CreateRequest<'_>,
) -> Result<Sources, Refusal> {
    if req.gender != 0 && req.gender != 1 {
        return Err(Refusal::Custom("Unknown gender.".into()));
    }

    let row = tx
        .query_opt(
            r#"SELECT b."raceID", t."typeID", b."shipTypeID",
                      b."intelligence", b."charisma", b."perception", b."memory", b."willpower",
                      b."skillTypeID1", b."skillTypeID2",
                      r."skillTypeID1", r."typeID", r."typeQuantity"
                 FROM "chrBloodlines" b
                 JOIN "bloodlineTypes" t ON t."bloodlineID" = b."bloodlineID"
                 JOIN "chrRaces" r ON r."raceID" = b."raceID"
                WHERE b."bloodlineID" = $1 AND b."raceID" IN (1,2,4,8)"#,
            &[&req.bloodline_id],
        )
        .await
        .map_err(|e| db_failed("bloodline lookup", e))?;
    let Some(row) = row else {
        return Err(Refusal::Custom("Unknown bloodline.".into()));
    };
    let race_id: i32 = row.get(0);
    let type_id: i32 = row.get(1);
    let ship_type_id: i32 = row.get(2);
    let base = Attrs {
        intelligence: row.get(3),
        charisma: row.get(4),
        perception: row.get(5),
        memory: row.get(6),
        willpower: row.get(7),
    };
    let mut skills: Vec<i32> = Vec::new();
    push_skill(&mut skills, row.get::<_, Option<i32>>(10));
    push_skill(&mut skills, row.get::<_, Option<i32>>(8));
    push_skill(&mut skills, row.get::<_, Option<i32>>(9));
    let charge_type_id: Option<i32> = row.get(11);
    let charge_quantity: Option<i32> = row.get(12);

    let row = tx
        .query_opt(
            r#"SELECT "postIntelligence","postCharisma","postPerception","postMemory","postWillpower",
                      "skillTypeID1","skillTypeID2"
                 FROM "chrAncestries" WHERE "ancestryID" = $1 AND "bloodlineID" = $2"#,
            &[&req.ancestry_id, &req.bloodline_id],
        )
        .await
        .map_err(|e| db_failed("ancestry lookup", e))?;
    let Some(row) = row else {
        return Err(Refusal::Custom("That ancestry does not belong to that bloodline.".into()));
    };
    let post = Attrs {
        intelligence: row.get(0),
        charisma: row.get(1),
        perception: row.get(2),
        memory: row.get(3),
        willpower: row.get(4),
    };
    push_skill(&mut skills, row.get::<_, Option<i32>>(5));
    push_skill(&mut skills, row.get::<_, Option<i32>>(6));

    let row = tx
        .query_opt(
            r#"SELECT s."corporationID", st."stationID",
                      st."solarSystemID", st."constellationID", st."regionID"
                 FROM "chrSchools" s
                 JOIN "crpNPCCorporations" n ON n."corporationID" = s."corporationID"
                 JOIN "staStations" st ON st."corporationID" = s."corporationID"
                                      AND st."solarSystemID" = n."solarSystemID"
                WHERE s."schoolID" = $1 AND s."raceID" = $2
                ORDER BY st."stationID"
                LIMIT 1"#,
            &[&req.school_id, &race_id],
        )
        .await
        .map_err(|e| db_failed("school lookup", e))?;
    let Some(row) = row else {
        return Err(Refusal::Custom("That school does not belong to that race.".into()));
    };
    let corporation_id: i32 = row.get(0);
    let station_id: i32 = row.get(1);
    let solar_system_id: i32 = row.get(2);
    let constellation_id: i32 = row.get(3);
    let region_id: i32 = row.get(4);

    let row = tx
        .query_opt(
            r#"SELECT "skillTypeID1","skillTypeID2","skillTypeID3"
                 FROM "chrDepartments" WHERE "departmentID" = $1 AND "schoolID" = $2"#,
            &[&req.department_id, &req.school_id],
        )
        .await
        .map_err(|e| db_failed("department lookup", e))?;
    let Some(row) = row else {
        return Err(Refusal::Custom("That department does not belong to that school.".into()));
    };
    push_skill(&mut skills, row.get::<_, Option<i32>>(0));
    push_skill(&mut skills, row.get::<_, Option<i32>>(1));
    push_skill(&mut skills, row.get::<_, Option<i32>>(2));

    let mut equipment: Vec<(i32, i32)> = Vec::new();
    let row = tx
        .query_opt(
            r#"SELECT "skillTypeID1","skillTypeID2","typeID","typeQuantity"
                 FROM "chrFields" WHERE "fieldID" = $1 AND "departmentID" = $2"#,
            &[&req.field_id, &req.department_id],
        )
        .await
        .map_err(|e| db_failed("field lookup", e))?;
    let Some(row) = row else {
        return Err(Refusal::Custom("That field does not belong to that department.".into()));
    };
    push_skill(&mut skills, row.get::<_, Option<i32>>(0));
    push_skill(&mut skills, row.get::<_, Option<i32>>(1));
    push_equipment(&mut equipment, row.get::<_, Option<i32>>(2), row.get::<_, Option<i32>>(3));

    let row = tx
        .query_opt(
            r#"SELECT "skillTypeID1","skillTypeID2","typeID","typeQuantity"
                 FROM "chrSpecialities" WHERE "specialityID" = $1 AND "fieldID" = $2"#,
            &[&req.speciality_id, &req.field_id],
        )
        .await
        .map_err(|e| db_failed("speciality lookup", e))?;
    let Some(row) = row else {
        return Err(Refusal::Custom("That speciality does not belong to that field.".into()));
    };
    push_skill(&mut skills, row.get::<_, Option<i32>>(0));
    push_skill(&mut skills, row.get::<_, Option<i32>>(1));
    push_equipment(&mut equipment, row.get::<_, Option<i32>>(2), row.get::<_, Option<i32>>(3));

    Ok(Sources {
        type_id,
        race_id,
        ship_type_id,
        corporation_id,
        station_id,
        solar_system_id,
        constellation_id,
        region_id,
        base,
        post,
        charge_type_id,
        charge_quantity,
        skills: group_skills(&skills),
        equipment,
    })
}

fn push_skill(into: &mut Vec<i32>, id: Option<i32>) {
    if let Some(id) = id.filter(|v| *v != 0) {
        into.push(id);
    }
}

fn push_equipment(into: &mut Vec<(i32, i32)>, type_id: Option<i32>, quantity: Option<i32>) {
    if let Some(type_id) = type_id.filter(|v| *v != 0) {
        into.push((type_id, quantity.unwrap_or(1).max(1)));
    }
}

fn group_skills(all: &[i32]) -> Vec<(i32, i32)> {
    let mut out: Vec<(i32, i32)> = Vec::new();
    for id in all {
        match out.iter_mut().find(|(t, _)| t == id) {
            Some((_, level)) => *level += 1,
            None => out.push((*id, 1)),
        }
    }
    for (id, level) in out.iter_mut() {
        if *level > 5 {
            warn!("CreateCharacter: skill {id} granted {level} times, clamped to 5");
            *level = 5;
        }
    }
    out
}

fn validate_points(spent: &Spent, src: &Sources) -> Result<(), Refusal> {
    let values = spent.all();
    if values.iter().any(|v| !(0..=MAX_POINTS_PER_ATTRIBUTE).contains(v)) {
        return Err(Refusal::Custom(format!(
            "No attribute may take more than {MAX_POINTS_PER_ATTRIBUTE} points."
        )));
    }
    if values.iter().sum::<i32>() > POINTS_TO_ASSIGN {
        return Err(Refusal::Custom(format!(
            "Only {POINTS_TO_ASSIGN} attribute points may be assigned."
        )));
    }
    let bases = [
        src.base.intelligence,
        src.base.charisma,
        src.base.perception,
        src.base.memory,
        src.base.willpower,
    ];
    if bases.iter().zip(values).any(|(b, v)| b + v > MAX_ATTRIBUTE_VALUE) {
        return Err(Refusal::Custom(format!(
            "No attribute may exceed {MAX_ATTRIBUTE_VALUE}."
        )));
    }
    Ok(())
}

struct Appearance {
    ids: Vec<Option<i32>>,
    rotations: Vec<f64>,
    morphs: Vec<Option<f64>>,
}

async fn read_appearance(
    tx: &tokio_postgres::Transaction<'_>,
    req: &CreateRequest<'_>,
) -> Result<Appearance, Refusal> {
    let mut ids = Vec::with_capacity(SLOTS.len());
    for s in SLOTS {
        let key = format!("{}ID", s.stem);
        let value = dict_get(req.appearance, &key);
        let id = match value {
            None | Some(Value::None) => None,
            Some(v) => match v.as_i64().and_then(|v| i32::try_from(v).ok()) {
                Some(id) => Some(id),
                None => {
                    return Err(Refusal::Custom(format!("Appearance value {key} is not an id.")))
                }
            },
        };
        match id {
            None if s.required => {
                return Err(Refusal::Custom(format!("Appearance is missing {key}.")));
            }
            None => {}
            Some(id) => {
                let found = if s.keyed {
                    tx.query_opt(s.sql, &[&id, &req.bloodline_id, &req.gender]).await
                } else {
                    tx.query_opt(s.sql, &[&id]).await
                }
                .map_err(|e| db_failed("appearance check", e))?;
                if found.is_none() {
                    return Err(Refusal::Custom(format!(
                        "Appearance value {key} = {id} is not available to this bloodline."
                    )));
                }
            }
        }
        ids.push(id);
    }

    let mut rotations = Vec::with_capacity(ROTATIONS.len());
    for key in ROTATIONS {
        let Some(v) = dict_get(req.appearance, key).and_then(Value::as_f64) else {
            return Err(Refusal::Custom(format!("Appearance is missing {key}.")));
        };
        rotations.push(v);
    }

    let mut morphs = Vec::with_capacity(MORPHS.len());
    for key in MORPHS {
        morphs.push(dict_get(req.appearance, key).and_then(Value::as_f64));
    }

    Ok(Appearance { ids, rotations, morphs })
}

async fn insert_character_row(
    tx: &tokio_postgres::Transaction<'_>,
    req: &CreateRequest<'_>,
    src: &Sources,
    a: &Appearance,
    char_id: i32,
    now: i64,
) -> Result<(), Refusal> {
    let gender = req.gender as i16;
    let empty = "";
    tx.execute(
        r#"INSERT INTO "character_" (
             "characterID","characterName","accountID","title","description",
             "bounty","balance","securityRating","petitionMessage","logonMinutes",
             "corporationID","corporationDateTime","startDateTime","createDateTime",
             "typeID","raceID","bloodlineID","gender",
             "accessoryID","beardID","costumeID","decoID","eyebrowsID","eyesID","hairID",
             "lipstickID","makeupID","skinID","backgroundID","lightID",
             "headRotation1","headRotation2","headRotation3",
             "eyeRotation1","eyeRotation2","eyeRotation3",
             "camPos1","camPos2","camPos3",
             "morph1e","morph1n","morph1s","morph1w",
             "morph2e","morph2n","morph2s","morph2w",
             "morph3e","morph3n","morph3s","morph3w",
             "morph4e","morph4n","morph4s","morph4w",
             "stationID","solarSystemID","constellationID","regionID")
           VALUES (
             $1,$2,$3,$4,$5,
             0,$6,0,$7,0,
             $8,$9,$9,$9,
             $10,$11,$12,$13,
             $14,$15,$16,$17,$18,$19,$20,
             $21,$22,$23,$24,$25,
             $26,$27,$28,
             $29,$30,$31,
             $32,$33,$34,
             $35,$36,$37,$38,
             $39,$40,$41,$42,
             $43,$44,$45,$46,
             $47,$48,$49,$50,
             $51,$52,$53,$54)"#,
        &[
            &char_id,
            &req.name.as_str(),
            &req.account_id,
            &empty,
            &empty,
            &crate::config::starting_isk(req.account_id as i64),
            &empty,
            &src.corporation_id,
            &now,
            &src.type_id,
            &src.race_id,
            &req.bloodline_id,
            &gender,
            &a.ids[0], &a.ids[1], &a.ids[2], &a.ids[3], &a.ids[4], &a.ids[5], &a.ids[6],
            &a.ids[7], &a.ids[8], &a.ids[9], &a.ids[10], &a.ids[11],
            &a.rotations[0], &a.rotations[1], &a.rotations[2],
            &a.rotations[3], &a.rotations[4], &a.rotations[5],
            &a.rotations[6], &a.rotations[7], &a.rotations[8],
            &a.morphs[0], &a.morphs[1], &a.morphs[2], &a.morphs[3],
            &a.morphs[4], &a.morphs[5], &a.morphs[6], &a.morphs[7],
            &a.morphs[8], &a.morphs[9], &a.morphs[10], &a.morphs[11],
            &a.morphs[12], &a.morphs[13], &a.morphs[14], &a.morphs[15],
            &src.station_id,
            &src.solar_system_id,
            &src.constellation_id,
            &src.region_id,
        ],
    )
    .await
    .map_err(|e| db_failed("character_ insert", e))?;
    Ok(())
}

fn arg_i32(call: &Call, i: usize) -> Option<i32> {
    call.arg_i64(i).and_then(|v| i32::try_from(v).ok())
}

fn dict_get<'a>(d: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    d.iter()
        .find(|(k, _)| match k {
            Value::Bytes(b) | Value::Buffer(b) => b.as_slice() == key.as_bytes(),
            Value::Unicode(s) => s == key,
            _ => false,
        })
        .map(|(_, v)| v)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_shape_matches_the_client_rules() {
        assert!(name_shape_ok("Test Pilot"));
        assert!(name_shape_ok("Abcd"));
        assert!(!name_shape_ok("Abc"));
        assert!(name_shape_ok(&"a".repeat(24)));
        assert!(!name_shape_ok(&"a".repeat(25)));
        assert!(!name_shape_ok("One Two Three"));

        assert!(!name_shape_ok("ÆØÅé"));
        assert_eq!("ÆØÅé".len(), 8, "the byte length differs from the character length");
        assert_eq!("ÆØÅé".chars().count(), 4);

        assert!(name_shape_ok("Jean-Luc Picard"));
        assert!(name_shape_ok("O'Brien"));
        assert!(name_shape_ok("Pilot7"));
        assert!(!name_shape_ok("Test_Pilot"));
        assert!(!name_shape_ok("Test.Pilot"));
        assert!(!name_shape_ok(" Leading"));
        assert!(!name_shape_ok("Trailing "));
        assert!(!name_shape_ok("-Dashed"));
        assert!(!name_shape_ok("Dashed-"));
        assert!(!name_shape_ok(""));
    }

    #[test]
    fn name_failures_map_to_the_right_message_id() {
        for (name, expected) in [
            ("Abc", "CharNameInvalidMinLength"),
            ("aaaaaaaaaaaaaaaaaaaaaaaaa", "CharNameInvalidMaxLength"),
            ("One Two Three", "CharNameInvalidMaxSpaces"),
            ("Test_Pilot", "CharNameInvalidSomeChar"),
            (" Leading", "CharNameInvalidFirstChar"),
            ("Trailing ", "CharNameInvalidLastChar"),
        ] {
            let got = name_shape_error(name);
            assert_eq!(got, Some(expected), "{name:?}");
            assert!(
                got.unwrap().starts_with("CharNameInvalid"),
                "the prefix is what keeps the wizard open"
            );
        }
        assert_eq!(name_shape_error("Test Pilot"), None);
        assert_eq!(name_shape_error("_"), Some("CharNameInvalidMinLength"));
    }

    #[test]
    fn skill_level_is_multiplicity() {
        assert_eq!(group_skills(&[3300, 3301, 3300, 3302, 3300]), vec![
            (3300, 3),
            (3301, 1),
            (3302, 1)
        ]);
        assert_eq!(group_skills(&[]), vec![]);
        assert_eq!(group_skills(&[7, 7, 7, 7, 7, 7, 7]), vec![(7, 5)]);
    }

    #[test]
    fn shipped_name_forms_are_accepted() {
        for name in ["EVE System", "Antaken Kamola", "Ansanaisen Eskonainen", "Appi Intaa"] {
            assert!(name_shape_ok(name), "{name:?} is a shipped name and must validate");
        }
    }
}

const APPEARANCE_HEADER: &[&str] = &[
    "gender", "accessoryID", "beardID", "costumeID", "decoID", "eyebrowsID", "eyesID", "hairID",
    "lipstickID", "makeupID", "skinID", "backgroundID", "lightID", "headRotation1",
    "headRotation2", "headRotation3", "eyeRotation1", "eyeRotation2", "eyeRotation3", "camPos1",
    "camPos2", "camPos3", "morph1e", "morph1n", "morph1s", "morph1w", "morph2e", "morph2n",
    "morph2s", "morph2w", "morph3e", "morph3n", "morph3s", "morph3w", "morph4e", "morph4n",
    "morph4s", "morph4w",
];

async fn character_appearance_list(ctx: &Ctx<'_>, ids: &[i32]) -> Value {
    if ids.is_empty() {
        return Value::List(Vec::new());
    }
    let sql = r#"SELECT "characterID","gender","accessoryID","beardID","costumeID","decoID",
                        "eyebrowsID","eyesID","hairID","lipstickID","makeupID","skinID",
                        "backgroundID","lightID","headRotation1","headRotation2","headRotation3",
                        "eyeRotation1","eyeRotation2","eyeRotation3","camPos1","camPos2","camPos3",
                        "morph1e","morph1n","morph1s","morph1w","morph2e","morph2n","morph2s",
                        "morph2w","morph3e","morph3n","morph3s","morph3w","morph4e","morph4n",
                        "morph4s","morph4w"
                   FROM "character_" WHERE "characterID" = ANY($1)"#;
    let rows = match ctx.db.query(sql, &[&ids]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("character.GetCharacterAppearanceList: {err}", err = crate::logging::chain(&e));
            Vec::new()
        }
    };
    let out = ids
        .iter()
        .map(|id| match rows.iter().find(|r| r.get::<_, i32>(0) == *id) {
            Some(r) => {
                let line: Vec<Value> =
                    (1..=APPEARANCE_HEADER.len()).map(|i| crate::svc::rows::cell(r, i)).collect();
                crate::svc::rows::rowset(APPEARANCE_HEADER, vec![Value::List(line)])
            }
            None => Value::Dict(Vec::new()),
        })
        .collect();
    Value::List(out)
}
