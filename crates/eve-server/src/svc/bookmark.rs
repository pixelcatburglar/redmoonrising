use eve_marshal::Value;
use tracing::{error, warn};

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{cell, rowset};

const BOOKMARK_HEADER: &[&str] = &[
    "bookmarkID", "ownerID", "itemID", "typeID", "flag", "memo", "created", "x", "y", "z",
    "locationID",
];

const BOOKMARK_SELECT: &str = r#"SELECT "bookmarkID","ownerID","itemID","typeID","flag","memo",
                                        "created","x","y","z","locationID"
                                   FROM bookmarks"#;

pub async fn bookmark(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let arg = |i: usize| call.arg_i64(i);
    let value = match call.method.as_str() {
        "GetBookmarks" => get_bookmarks(ctx).await,
        "BookmarkLocation" => {
            bookmark_location(ctx, arg(0).unwrap_or(0), call.arg_text(2)).await
        }
        "UpdateBookmark" => update_bookmark(ctx, arg(0).unwrap_or(0), call.arg_text(2)).await,
        "DeleteBookmarks" => delete_bookmarks(ctx, call.args.first()).await,

        "CreateBookmarkVoucher" | "CreateBookmarkVouchers" | "AddBookmarkFromVoucher" => {
            return Some(Outcome::Unknown(format!(
                "bookmark::{} needs the voucher entity path (recon/12 §4.5)",
                call.method
            )))
        }

        _ => return None,
    };
    Some(Outcome::Value(value))
}

async fn get_bookmarks(ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return rowset(BOOKMARK_HEADER, Vec::new()) };
    let sql = format!(r#"{BOOKMARK_SELECT} WHERE "ownerID" = $1 ORDER BY "bookmarkID""#);
    ctx.rowset(BOOKMARK_HEADER, sql.as_str(), &[&(char_id as i32)]).await
}

async fn bookmark_location(ctx: &Ctx<'_>, item_id: i64, memo: Option<String>) -> Value {
    let failed = || {
        Value::Tuple(vec![
            Value::Int(0),
            Value::Int(item_id),
            Value::None,
            Value::None,
            Value::None,
            Value::None,
            Value::None,
        ])
    };
    let Some(char_id) = ctx.session.char_id else { return failed() };

    let sql = r#"SELECT "typeID","solarSystemID","x","y","z"
                   FROM "mapDenormalize" WHERE "itemID" = $1"#;
    let found = match ctx.db.query(sql, &[&(item_id as i32)]).await {
        Ok(rows) => rows.into_iter().next(),
        Err(e) => {
            error!("BookmarkLocation({item_id}) lookup: {err}", err = crate::logging::chain(&e));
            return failed();
        }
    };
    let Some(r) = found else {
        warn!(
            "BookmarkLocation({item_id}): not a mapDenormalize item -- a \
             raw-coordinate bookmark needs a ballpark position this server does not track"
        );
        return failed();
    };

    let type_id: i32 = r.get(0);
    let location_id: i32 = r
        .try_get::<_, Option<i32>>(1)
        .ok()
        .flatten()
        .or_else(|| ctx.session.solar_system_id2.and_then(|v| i32::try_from(v).ok()))
        .unwrap_or(0);
    let (x, y, z): (f64, f64, f64) = (r.get(2), r.get(3), r.get(4));

    let insert = r#"INSERT INTO bookmarks
                        ("ownerID","itemID","typeID","flag","memo","created","x","y","z","locationID")
                    VALUES ($1,$2,$3,NULL,$4,$5,$6,$7,$8,$9)
                    RETURNING "bookmarkID""#;
    let memo = memo.unwrap_or_default();
    let created = crate::bulkdata::filetime_now();
    match ctx
        .db
        .query_one(
            insert,
            &[
                &(char_id as i32),
                &(item_id as i32),
                &type_id,
                &memo,
                &created,
                &x,
                &y,
                &z,
                &location_id,
            ],
        )
        .await
    {
        Ok(row) => Value::Tuple(vec![
            cell(&row, 0),
            Value::Int(item_id),
            Value::Int(type_id as i64),
            Value::Double(x),
            Value::Double(y),
            Value::Double(z),
            Value::Int(location_id as i64),
        ]),
        Err(e) => {
            error!("BookmarkLocation({item_id}) insert: {err}", err = crate::logging::chain(&e));
            failed()
        }
    }
}

async fn update_bookmark(ctx: &Ctx<'_>, bookmark_id: i64, memo: Option<String>) -> Value {
    let (Some(char_id), Some(memo)) = (ctx.session.char_id, memo) else { return Value::None };
    let sql = r#"UPDATE bookmarks SET "memo" = $1 WHERE "bookmarkID" = $2 AND "ownerID" = $3"#;
    if let Err(e) = ctx.db.execute(sql, &[&memo, &(bookmark_id as i32), &(char_id as i32)]).await {
        error!("UpdateBookmark({bookmark_id}): {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

async fn delete_bookmarks(ctx: &Ctx<'_>, arg: Option<&Value>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let ids: Vec<i32> = arg
        .and_then(|v| v.seq())
        .map(|s| s.iter().filter_map(|v| v.as_i64()).filter_map(|v| i32::try_from(v).ok()).collect())
        .unwrap_or_default();
    if ids.is_empty() {
        return Value::None;
    }
    let sql = r#"DELETE FROM bookmarks
                  WHERE "bookmarkID" = ANY($1) AND "ownerID" = $2"#;
    if let Err(e) = ctx.db.execute(sql, &[&ids, &(char_id as i32)]).await {
        error!("DeleteBookmarks({} ids): {err}", ids.len(), err = crate::logging::chain(&e));
    }
    Value::None
}

