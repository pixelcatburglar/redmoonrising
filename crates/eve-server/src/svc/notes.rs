use eve_marshal::Value;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{cell, rowset};

pub async fn character(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let arg = |i: usize| call.arg_i64(i);
    let value = match call.method.as_str() {
        "GetOwnerNoteLabels" => owner_note_labels(ctx).await,
        "GetOwnerNote" => owner_note(ctx, arg(0).unwrap_or(0)).await,
        "AddOwnerNote" => {
            add_owner_note(ctx, call.arg_text(0), call.arg_text(1)).await
        }
        "EditOwnerNote" => {
            edit_owner_note(ctx, arg(0).unwrap_or(0), call.arg_text(1), call.arg_text(2)).await
        }
        "RemoveOwnerNote" => remove_owner_note(ctx, arg(0).unwrap_or(0)).await,

        "GetNote" => get_note(ctx, arg(0).unwrap_or(0)).await,
        "SetNote" => set_note(ctx, arg(0).unwrap_or(0), call.arg_text(1)).await,

        _ => return None,
    };
    Some(Outcome::Value(value))
}

async fn owner_note_labels(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["noteID", "label"];
    let Some(char_id) = ctx.session.char_id else { return rowset(HEADER, Vec::new()) };
    let sql = r#"SELECT "noteID","label" FROM "chrOwnerNote"
                  WHERE "ownerID" = $1 AND "label" <> '' ORDER BY "noteID""#;
    ctx.rowset(HEADER, sql, &[&(char_id as i32)]).await
}

async fn owner_note(ctx: &Ctx<'_>, note_id: i64) -> Value {
    const HEADER: &[&str] = &["noteID", "label", "note"];
    let Some(char_id) = ctx.session.char_id else { return rowset(HEADER, Vec::new()) };
    let sql = r#"SELECT "noteID","label","note" FROM "chrOwnerNote"
                  WHERE "noteID" = $1 AND "ownerID" = $2"#;
    ctx.rowset(HEADER, sql, &[&(note_id as i32), &(char_id as i32)]).await
}

async fn add_owner_note(ctx: &Ctx<'_>, label: Option<String>, note: Option<String>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let sql = r#"INSERT INTO "chrOwnerNote" ("ownerID","label","note")
                 VALUES ($1,$2,$3) RETURNING "noteID""#;
    let label = label.unwrap_or_default();
    let note = note.unwrap_or_default();
    match ctx.db.query_one(sql, &[&(char_id as i32), &label, &note]).await {
        Ok(row) => cell(&row, 0),
        Err(e) => {
            error!("AddOwnerNote({label:?}): {err}", err = crate::logging::chain(&e));
            Value::None
        }
    }
}

async fn edit_owner_note(
    ctx: &Ctx<'_>,
    note_id: i64,
    label: Option<String>,
    note: Option<String>,
) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let label = label.unwrap_or_default();
    let result = match note {
        Some(note) => {
            let sql = r#"UPDATE "chrOwnerNote" SET "label" = $1, "note" = $2
                          WHERE "noteID" = $3 AND "ownerID" = $4"#;
            ctx.db.execute(sql, &[&label, &note, &(note_id as i32), &(char_id as i32)]).await
        }
        None => {
            let sql = r#"UPDATE "chrOwnerNote" SET "label" = $1
                          WHERE "noteID" = $2 AND "ownerID" = $3"#;
            ctx.db.execute(sql, &[&label, &(note_id as i32), &(char_id as i32)]).await
        }
    };
    if let Err(e) = result {
        error!("EditOwnerNote({note_id}): {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

async fn remove_owner_note(ctx: &Ctx<'_>, note_id: i64) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let sql = r#"DELETE FROM "chrOwnerNote" WHERE "noteID" = $1 AND "ownerID" = $2"#;
    if let Err(e) = ctx.db.execute(sql, &[&(note_id as i32), &(char_id as i32)]).await {
        error!("RemoveOwnerNote({note_id}): {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

async fn get_note(ctx: &Ctx<'_>, item_id: i64) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::bytes("") };
    let sql = r#"SELECT "note" FROM "chrNotes" WHERE "characterID" = $1 AND "itemID" = $2"#;
    match ctx.db.query(sql, &[&(char_id as i32), &(item_id as i32)]).await {
        Ok(rows) => match rows.first() {
            Some(r) => cell(r, 0),
            None => Value::bytes(""),
        },
        Err(e) => {
            error!("GetNote({item_id}): {err}", err = crate::logging::chain(&e));
            Value::bytes("")
        }
    }
}

async fn set_note(ctx: &Ctx<'_>, item_id: i64, text: Option<String>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let text = text.unwrap_or_default();
    let sql = r#"INSERT INTO "chrNotes" ("characterID","itemID","note") VALUES ($1,$2,$3)
                 ON CONFLICT ("characterID","itemID") DO UPDATE SET "note" = EXCLUDED."note""#;
    if let Err(e) = ctx.db.execute(sql, &[&(char_id as i32), &(item_id as i32), &text]).await {
        error!("SetNote({item_id}): {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

