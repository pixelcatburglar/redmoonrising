//! The medical bay (medical.py): a character's clone and where it waits.
//!
//! The clone is an item of the Clone group the character owns, located on
//! the character. Its type is the grade (how many skill points it keeps);
//! its customInfo names the home station, where the pilot wakes when the
//! capsule is lost. Without one the pilot has an Alpha clone at their
//! corporation's station.

use eve_marshal::Value;
use tokio_postgres::GenericClient;
use tracing::{error, info};

use crate::services::{Ctx, Outcome};

pub const GROUP_CLONE: i32 = 23;
pub const TYPE_CLONE_ALPHA: i64 = 164;
/// market_refTypes: "Clone Activation".
const REF_CLONE: i32 = 8;

/// The station the clone waits in, if one was chosen.
pub async fn home_station(db: &impl GenericClient, char_id: i64) -> Result<Option<i64>, tokio_postgres::Error> {
    let row = db
        .query_opt(
            r#"SELECT e."customInfo" FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID"
                WHERE e."ownerID" = $1 AND t."groupID" = $2
                ORDER BY e."itemID" DESC LIMIT 1"#,
            &[&(char_id as i32), &GROUP_CLONE],
        )
        .await?;
    Ok(row.and_then(|r| r.get::<_, Option<String>>(0)).and_then(|s| s.trim().parse().ok()))
}

/// Gives the character a clone of `type_id` waiting at `home`, in place of
/// whatever clone it had.
pub async fn replace_clone(db: &impl GenericClient, char_id: i64, type_id: i64, home: Option<i64>) -> Result<(), tokio_postgres::Error> {
    db.execute(
        r#"DELETE FROM entity e USING "invTypes" t
            WHERE t."typeID" = e."typeID" AND e."ownerID" = $1 AND t."groupID" = $2"#,
        &[&(char_id as i32), &GROUP_CLONE],
    )
    .await?;
    db.execute(
        r#"INSERT INTO entity ("itemName","typeID","ownerID","locationID","flag","singleton","quantity","customInfo")
           VALUES ('', $2, $1, $1, 0, 1, 1, $3)"#,
        &[&(char_id as i32), &(type_id as i32), &home.map(|h| h.to_string())],
    )
    .await?;
    Ok(())
}

async fn clone_type(db: &impl GenericClient, char_id: i64) -> Result<Option<i64>, tokio_postgres::Error> {
    let row = db
        .query_opt(
            r#"SELECT e."typeID" FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID"
                WHERE e."ownerID" = $1 AND t."groupID" = $2 ORDER BY e."itemID" DESC LIMIT 1"#,
            &[&(char_id as i32), &GROUP_CLONE],
        )
        .await?;
    Ok(row.map(|r| r.get::<_, i32>(0) as i64))
}

/// character.GetHomeStation: the clone's station, else the corporation's.
pub async fn get_home_station(ctx: &Ctx<'_>) -> Value {
    let chosen = match ctx.session.char_id {
        Some(char_id) => home_station(ctx.db, char_id).await.unwrap_or_else(|e| {
            error!("character.GetHomeStation: {}", crate::logging::chain(&e));
            None
        }),
        None => None,
    };
    Value::int_or_none(chosen.or(ctx.session.hq_id).or(ctx.session.station_id))
}

/// corpStationMgr.SetHomeStation(stationID): move the clone to the station
/// the pilot is docked in.
pub async fn set_home_station(ctx: &Ctx<'_>, station_id: i64) -> Outcome {
    let Some(char_id) = ctx.session.char_id else { return Outcome::custom("No character.") };
    if ctx.session.station_id != Some(station_id) {
        return Outcome::custom("You can only move your clone to the station you are in.");
    }
    let result = async {
        let grade = clone_type(ctx.db, char_id).await?.unwrap_or(TYPE_CLONE_ALPHA);
        replace_clone(ctx.db, char_id, grade, Some(station_id)).await
    }
    .await;
    match result {
        Ok(()) => {
            info!("{char_id} moved their clone to {station_id}");
            Outcome::Value(Value::None)
        }
        Err(e) => {
            error!("SetHomeStation({station_id}): {}", crate::logging::chain(&e));
            Outcome::custom("Your clone could not be moved.")
        }
    }
}

/// corpStationMgr.SetCloneTypeID(typeID): buy a better clone; it keeps its
/// home station.
pub async fn set_clone_type(ctx: &Ctx<'_>, type_id: i64) -> Outcome {
    use crate::svc::wallet::{move_cash, Payment, Transfer};
    let Some(char_id) = ctx.session.char_id else { return Outcome::custom("No character.") };
    let sql = r#"SELECT "basePrice" FROM "invTypes" WHERE "typeID" = $1 AND "groupID" = $2"#;
    let Some(price) = ctx.query_one(sql, &[&(type_id as i32), &GROUP_CLONE]).await.map(|r| r.get::<_, f64>(0)) else {
        return Outcome::custom("That is not a clone.");
    };
    let arg = type_id.to_string();
    let payment = Payment { payer: char_id, payee: None, amount: price, ref_type: REF_CLONE, arg_id1: &arg, reason: "" };
    match move_cash(ctx.db_url, payment).await {
        Ok(()) => {}
        Err(Transfer::NotEnough { need, have }) => return Outcome::not_enough_money(need, have),
        Err(Transfer::NoSuchOwner) => return Outcome::custom("There is no wallet to pay from."),
        Err(Transfer::Db(e)) => {
            error!("buying clone {type_id}: {}", crate::logging::chain(&e));
            return Outcome::custom("The clone could not be paid for.");
        }
    }
    let result = async {
        let home = home_station(ctx.db, char_id).await?;
        replace_clone(ctx.db, char_id, type_id, home).await
    }
    .await;
    if let Err(e) = result {
        error!("installing clone {type_id} for {char_id}: {}", crate::logging::chain(&e));
        return Outcome::custom("The clone could not be installed.");
    }
    info!("{char_id} bought clone {type_id} for {price} ISK");
    Outcome::Value(Value::None)
}
