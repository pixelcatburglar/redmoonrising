//! The row containers the client unpickles (`util.Row`, `util.Rowset`,
//! `util.IndexRowset`) and NULL-safe reads from PostgreSQL rows into them.

use eve_marshal::Value;
use tokio_postgres::Row;

/// A `util.KeyVal`: a bag of named attributes.
pub fn key_val(fields: Vec<(&str, Value)>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("util.KeyVal")),
        state: Box::new(Value::Dict(
            fields.into_iter().map(|(k, v)| (Value::bytes(k), v)).collect(),
        )),
    }
}

pub fn row(header: &[&str], line: Vec<Value>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("util.Row")),
        state: Box::new(Value::Dict(vec![
            (Value::bytes("header"), header_value(header)),
            (Value::bytes("line"), Value::List(line)),
        ])),
    }
}

pub fn rowset(header: &[&str], lines: Vec<Value>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("util.Rowset")),
        state: Box::new(Value::Dict(vec![
            (Value::bytes("header"), header_value(header)),
            (Value::bytes("lines"), Value::List(lines)),
        ])),
    }
}

/// A rowset that also carries its header as `columns`, which the market UI reads
/// (marketsvc.py:373 `bids.columns.index('jumps')`).
pub fn rowset_with_columns(header: &[&str], lines: Vec<Value>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("util.Rowset")),
        state: Box::new(Value::Dict(vec![
            (Value::bytes("header"), header_value(header)),
            (Value::bytes("lines"), Value::List(lines)),
            (Value::bytes("columns"), header_value(header)),
        ])),
    }
}

pub fn index_rowset(header: &[&str], id_name: &str, items: Vec<(Value, Value)>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("util.IndexRowset")),
        state: Box::new(Value::Dict(vec![
            (Value::bytes("header"), header_value(header)),
            (Value::bytes("items"), Value::Dict(items)),
            (Value::bytes("idName"), Value::bytes(id_name)),
        ])),
    }
}

pub fn header_value(header: &[&str]) -> Value {
    Value::List(header.iter().map(|h| Value::bytes(*h)).collect())
}

pub fn header_of(stmt: &tokio_postgres::Statement) -> Value {
    Value::List(stmt.columns().iter().map(|c| Value::bytes(c.name())).collect())
}

pub fn line(r: &Row) -> Vec<Value> {
    (0..r.columns().len()).map(|i| cell(r, i)).collect()
}

pub fn lines(rows: &[Row]) -> Vec<Value> {
    rows.iter().map(|r| Value::List(line(r))).collect()
}

/// Column `i` as the value the client expects for its type; NULL (or a type this
/// does not know) becomes `None`. Never panics.
pub fn cell(r: &Row, i: usize) -> Value {
    use tokio_postgres::types::Type;
    let Some(col) = r.columns().get(i) else { return Value::None };
    let get_i = |v: Option<i64>| v.map(Value::Int);
    let v = match *col.type_() {
        Type::INT2 => get_i(r.try_get::<_, Option<i16>>(i).ok().flatten().map(i64::from)),
        Type::INT4 => get_i(r.try_get::<_, Option<i32>>(i).ok().flatten().map(i64::from)),
        Type::INT8 => r.try_get::<_, Option<i64>>(i).ok().flatten().map(Value::Long),
        Type::FLOAT4 => r.try_get::<_, Option<f32>>(i).ok().flatten().map(|v| Value::Double(v.into())),
        Type::FLOAT8 => r.try_get::<_, Option<f64>>(i).ok().flatten().map(Value::Double),
        Type::BOOL => r.try_get::<_, Option<bool>>(i).ok().flatten().map(Value::Bool),
        _ => r.try_get::<_, Option<String>>(i).ok().flatten().map(Value::text),
    };
    v.unwrap_or(Value::None)
}

/// Column `i` as an integer, whatever integer type it is stored as. `None` for NULL.
pub fn int(r: &Row, i: usize) -> Option<i64> {
    cell(r, i).as_i64()
}

/// Column `i` as a float (integer columns widen). `None` for NULL.
pub fn float(r: &Row, i: usize) -> Option<f64> {
    cell(r, i).as_f64()
}

/// Column `i` as text. `None` for NULL.
pub fn text(r: &Row, i: usize) -> Option<String> {
    cell(r, i).as_string()
}
