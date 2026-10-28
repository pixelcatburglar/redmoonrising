//! Buying and selling on the market: marketProxy PlaceCharOrder,
//! CancelCharOrder and ModifyCharOrder (marketsvc.py:236-250).
//!
//! An order first trades against what is already on the market at its price or
//! better; with a duration of 0 ("immediate") it must fill completely or nothing
//! happens, otherwise what is left is listed. Buyers pay the seller's price.
//! NPC orders (owned by an NPC corporation) never run out of money: their buy
//! orders hold no escrow, and what they buy leaves the game. Corporation
//! orders are not supported yet.

use std::collections::HashMap;

use eve_marshal::Value;
use tokio_postgres::Row;
use tracing::{error, info};

use crate::items::{self, ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::svc::inventory::FLAG_HANGAR;
use crate::svc::market::{order_line, ORDER_HEADER, ORDER_SELECT};
use crate::svc::rows::row;
use crate::svc::wallet::{post, Entry, Transfer};

const RANGE_STATION: i64 = -1;
const ORDER_OPEN: i32 = 0;
/// const.mktOrderExpired, also used for an order that has run out.
const ORDER_DONE: i32 = 2;
const ORDER_CANCELLED: i32 = 3;
/// wallet.py:1045: 0 is money in (a sale), 1 money out (a purchase).
const TRANSACTION_SALE: i32 = 0;
const TRANSACTION_PURCHASE: i32 = 1;

const REF_MARKET_TRANSACTION: i32 = 2;
const REF_MARKET_ESCROW: i32 = 42;
const REF_BROKER_FEE: i32 = 46;
const REF_TRANSACTION_TAX: i32 = 54;
/// const.marketCommissionPercentage, const.mktTransactionTax, const.mktMinimumFee.
const BROKER_FEE: f64 = 0.01;
const TRANSACTION_TAX: f64 = 0.01;
const MINIMUM_FEE: f64 = 100.0;
const SKILL_BROKER_RELATIONS: i32 = 3446;
const SKILL_ACCOUNTING: i32 = 16622;
const CATEGORY_SHIP: i64 = 6;
const FIRST_PLAYER_ID: i64 = 100_000_000;

/// Why an order was not placed.
enum Refusal {
    Text(String),
    Money { need: f64, have: f64 },
    Db(tokio_postgres::Error),
}

impl From<tokio_postgres::Error> for Refusal {
    fn from(e: tokio_postgres::Error) -> Self {
        Refusal::Db(e)
    }
}

impl From<Transfer> for Refusal {
    fn from(t: Transfer) -> Self {
        match t {
            Transfer::NotEnough { need, have } => Refusal::Money { need, have },
            Transfer::NoSuchOwner => Refusal::Text("That wallet does not exist.".into()),
            Transfer::Db(e) => Refusal::Db(e),
        }
    }
}

fn refuse<T>(why: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal::Text(why.into()))
}

/// What the client is told once a trade commits.
#[derive(Default)]
struct Effects {
    item_events: Vec<Value>,
    /// Owners whose wallets changed, with their new balances.
    balances: HashMap<i64, f64>,
    /// Orders that changed, with the reason the client is given.
    orders: Vec<(i64, &'static str)>,
}

pub async fn market_write(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let Some(me) = ctx.session.char_id else { return Some(Outcome::custom("Select a character first.")) };
    let result = match call.method.as_str() {
        // PlaceCharOrder(stationID, typeID, price, quantity, bid, range, itemID,
        //                minVolume, duration, useCorp, located)
        "PlaceCharOrder" => {
            let order = NewOrder {
                station: call.arg_id(0),
                type_id: call.arg_id(1),
                price: call.args.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0),
                quantity: call.arg_id(3),
                bid: call.arg_id(4) != 0,
                range: call.arg_i64(5).unwrap_or(RANGE_STATION),
                item_id: call.arg_i64(6),
                min_volume: call.arg_i64(7).unwrap_or(1).max(1),
                duration: call.arg_id(8),
                corp: call.arg_id(9) != 0,
            };
            transact(ctx, |txn, fx| Box::pin(place(txn, fx, me, order))).await
        }
        // CancelCharOrder(orderID, regionID)
        "CancelCharOrder" => {
            let order_id = call.arg_id(0);
            transact(ctx, |txn, fx| Box::pin(cancel(txn, fx, me, order_id))).await
        }
        // ModifyCharOrder(orderID, newPrice, bid, stationID, solarSystemID, price, volRemaining, issued)
        "ModifyCharOrder" => {
            let order_id = call.arg_id(0);
            let price = call.args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
            transact(ctx, |txn, fx| Box::pin(modify(txn, fx, me, order_id, price))).await
        }
        _ => return None,
    };
    Some(result)
}

type Work<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), Refusal>> + Send + 'a>>;

async fn transact<F>(ctx: &Ctx<'_>, work: F) -> Outcome
where
    F: for<'a> FnOnce(&'a mut ItemTxn<'_>, &'a mut Effects) -> Work<'a>,
{
    let mut fx = Effects::default();
    let result: Result<(), Refusal> = async {
        let mut client = crate::db::connect(ctx.db_url).await?;
        let mut txn = ItemTxn::begin(&mut client).await?;
        work(&mut txn, &mut fx).await?;
        fx.item_events = txn.commit().await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            tell(ctx, fx).await;
            Outcome::Value(Value::None)
        }
        Err(Refusal::Text(why)) => {
            info!("market order refused: {why}");
            Outcome::custom(why)
        }
        Err(Refusal::Money { need, have }) => Outcome::not_enough_money(need, have),
        Err(Refusal::Db(e)) => {
            error!("market order: {}", crate::logging::chain(&e));
            Outcome::custom("The market could not process that order.")
        }
    }
}

/// Send item, wallet and order changes to whoever they concern.
async fn tell(ctx: &Ctx<'_>, fx: Effects) {
    let ids: Vec<i32> = fx.orders.iter().map(|(id, _)| *id as i32).collect();
    let sql = format!(r#"{ORDER_SELECT} WHERE o."orderID" = ANY($1)"#);
    let rows: HashMap<i64, Row> =
        ctx.query(&sql, &[&ids]).await.into_iter().map(|r| (r.get::<_, i32>(0) as i64, r)).collect();

    items::notify(ctx.world, ctx.client_id, fx.item_events);
    let world = ctx.world.lock();
    for (owner, balance) in fx.balances {
        let event = Value::Tuple(vec![Value::bytes("cash"), Value::Int(owner), Value::Double(balance)]);
        world.notify(&world.clients_of(owner), "OnAccountChange", event);
    }
    for (order_id, reason) in fx.orders {
        let Some(r) = rows.get(&order_id) else { continue };
        let owner = r.get::<_, i32>(2) as i64;
        let event = Value::Tuple(vec![row(ORDER_HEADER, order_line(r)), Value::bytes(reason)]);
        world.notify(&world.clients_of(owner), "OnOwnOrderChanged", event);
    }
}

struct NewOrder {
    station: i64,
    type_id: i64,
    price: f64,
    quantity: i64,
    bid: bool,
    range: i64,
    item_id: Option<i64>,
    min_volume: i64,
    duration: i64,
    corp: bool,
}

/// An order on the other side of a trade.
struct Match {
    id: i64,
    owner: i64,
    station: i64,
    price: f64,
    remaining: i64,
    escrow: f64,
}

struct Where {
    solar_system: i64,
    region: i64,
}

async fn place(txn: &mut ItemTxn<'_>, fx: &mut Effects, me: i64, order: NewOrder) -> Result<(), Refusal> {
    if order.corp {
        return refuse("Corporation orders are not available yet.");
    }
    if order.quantity <= 0 || !order.price.is_finite() || order.price <= 0.0 {
        return refuse("Enter a quantity and a price.");
    }
    let r = txn
        .tx()
        .query_opt(
            r#"SELECT "solarSystemID","regionID" FROM "staStations" WHERE "stationID" = $1"#,
            &[&(order.station as i32)],
        )
        .await?;
    let Some(r) = r else { return refuse("That station has no market.") };
    let at = Where { solar_system: r.get::<_, i32>(0) as i64, region: r.get::<_, i32>(1) as i64 };
    if order.bid {
        buy(txn, fx, me, &order, &at).await
    } else {
        sell(txn, fx, me, &order, &at).await
    }
}

async fn buy(txn: &mut ItemTxn<'_>, fx: &mut Effects, me: i64, order: &NewOrder, at: &Where) -> Result<(), Refusal> {
    // Sell orders inside the buy order's range, cheapest first.
    let rows = txn
        .tx()
        .query(
            r#"SELECT "orderID","charID","stationID","price","volRemaining","escrow"
                 FROM market_orders
                WHERE "typeID" = $1 AND "regionID" = $2 AND "bid" = 0 AND "orderState" = 0
                  AND "volRemaining" > 0 AND "price" <= $3 AND "charID" <> $4
                  AND ($5 > 0 OR ($5 = -1 AND "stationID" = $6) OR ($5 = 0 AND "solarSystemID" = $7))
                ORDER BY "price", "issued"
                  FOR UPDATE"#,
            &[
                &(order.type_id as i32),
                &(at.region as i32),
                &(order.price + 0.005),
                &(me as i32),
                &(order.range.max(-1) as i32),
                &(order.station as i32),
                &(at.solar_system as i32),
            ],
        )
        .await?;
    let fills = take(rows.iter().map(as_match), order.quantity);
    let filled: i64 = fills.iter().map(|(_, n)| n).sum();
    if order.duration == 0 && filled < order.quantity {
        return refuse("There is not enough for sale at that price within that range.");
    }

    for (m, n) in &fills {
        let amount = m.price * *n as f64;
        let entry = Entry { ref_type: REF_MARKET_TRANSACTION, payer: me, payee: m.owner, arg_id1: "", reason: "" };
        fx.balances.insert(me, post(txn.tx(), me, -amount, &entry).await?);
        fx.balances.insert(m.owner, post(txn.tx(), m.owner, amount, &entry).await?);
        if m.owner >= FIRST_PLAYER_ID {
            pay_tax(txn, fx, m.owner, amount).await?;
            record(txn, m.owner, me, order.type_id, *n, m.price, TRANSACTION_SALE, m.station, at.region).await?;
        }
        record(txn, me, m.owner, order.type_id, *n, m.price, TRANSACTION_PURCHASE, m.station, at.region).await?;
        let stack = NewItem { type_id: order.type_id, owner_id: me, location_id: m.station, flag: FLAG_HANGAR.into(), quantity: *n, singleton: false };
        txn.stack_onto(stack).await?;
        fill(txn, fx, m, *n, None).await?;
    }

    let left = order.quantity - filled;
    if left > 0 {
        let escrow = order.price * left as f64;
        broker_fee(txn, fx, me, escrow).await?;
        let entry = Entry { ref_type: REF_MARKET_ESCROW, payer: me, payee: 0, arg_id1: "", reason: "" };
        fx.balances.insert(me, post(txn.tx(), me, -escrow, &entry).await?);
        let id = list(txn, me, order, at, left, escrow).await?;
        fx.orders.push((id, "Add"));
    }
    info!("bought {filled} of type {} (listed {left})", order.type_id);
    Ok(())
}

async fn sell(txn: &mut ItemTxn<'_>, fx: &mut Effects, me: i64, order: &NewOrder, at: &Where) -> Result<(), Refusal> {
    let Some(item_id) = order.item_id else { return refuse("Choose the item to sell.") };
    let Some(item) = txn.item(item_id).await? else { return refuse("That item no longer exists.") };
    if item.owner_id != me || item.location_id != order.station || item.flag != FLAG_HANGAR as i64 {
        return refuse("Items must be in your hangar at that station to be sold there.");
    }
    if item.type_id != order.type_id || order.quantity > item.quantity {
        return refuse("You do not have that many to sell.");
    }
    if item.category_id == CATEGORY_SHIP && item.singleton {
        return refuse("Repackage the ship before selling it.");
    }
    let holds = txn.tx().query_opt(r#"SELECT 1 FROM entity WHERE "locationID" = $1 LIMIT 1"#, &[&(item_id as i32)]).await?;
    if holds.is_some() {
        return refuse("Empty the item before selling it.");
    }

    // Buy orders whose range reaches this station, best price first.
    let rows = txn
        .tx()
        .query(
            r#"SELECT "orderID","charID","stationID","price","volRemaining","escrow"
                 FROM market_orders
                WHERE "typeID" = $1 AND "regionID" = $2 AND "bid" = 1 AND "orderState" = 0
                  AND "volRemaining" > 0 AND "price" >= $3 AND "charID" <> $4
                  AND "minVolume" <= $5
                  AND ("range" > 0 OR ("range" = -1 AND "stationID" = $6)
                       OR ("range" = 0 AND "solarSystemID" = $7))
                ORDER BY "price" DESC, "issued"
                  FOR UPDATE"#,
            &[
                &(order.type_id as i32),
                &(at.region as i32),
                &(order.price - 0.005),
                &(me as i32),
                &(order.quantity as i32),
                &(order.station as i32),
                &(at.solar_system as i32),
            ],
        )
        .await?;
    let fills = take(rows.iter().map(as_match), order.quantity);
    let sold: i64 = fills.iter().map(|(_, n)| n).sum();
    if order.duration == 0 && sold < order.quantity {
        return refuse("Nobody is buying that many at that price within reach.");
    }

    for (m, n) in &fills {
        let amount = m.price * *n as f64;
        // Paid out of the buyer's escrow, or by an NPC buyer.
        let entry = Entry { ref_type: REF_MARKET_TRANSACTION, payer: m.owner, payee: me, arg_id1: "", reason: "" };
        fx.balances.insert(me, post(txn.tx(), me, amount, &entry).await?);
        pay_tax(txn, fx, me, amount).await?;
        record(txn, me, m.owner, order.type_id, *n, m.price, TRANSACTION_SALE, order.station, at.region).await?;
        if m.owner >= FIRST_PLAYER_ID {
            record(txn, m.owner, me, order.type_id, *n, m.price, TRANSACTION_PURCHASE, m.station, at.region).await?;
            let stack = NewItem { type_id: order.type_id, owner_id: m.owner, location_id: m.station, flag: FLAG_HANGAR.into(), quantity: *n, singleton: false };
            txn.stack_onto(stack).await?;
        }
        fill(txn, fx, m, *n, Some(amount)).await?;
    }

    let left = order.quantity - sold;
    if left > 0 {
        broker_fee(txn, fx, me, order.price * left as f64).await?;
        let id = list(txn, me, order, at, left, 0.0).await?;
        fx.orders.push((id, "Add"));
    }
    txn.set_quantity(item.id, item.quantity - order.quantity).await?;
    info!("sold {sold} of type {} (listed {left})", order.type_id);
    Ok(())
}

fn as_match(r: &Row) -> Match {
    Match {
        id: r.get::<_, i32>(0) as i64,
        owner: r.get::<_, i32>(1) as i64,
        station: r.get::<_, i32>(2) as i64,
        price: r.get(3),
        remaining: r.get::<_, i32>(4) as i64,
        escrow: r.get(5),
    }
}

/// Walk `orders` until `wanted` units are covered.
fn take(orders: impl Iterator<Item = Match>, wanted: i64) -> Vec<(Match, i64)> {
    let mut left = wanted;
    let mut out = Vec::new();
    for m in orders {
        if left == 0 {
            break;
        }
        let n = left.min(m.remaining);
        left -= n;
        out.push((m, n));
    }
    out
}

/// Take `n` units off an order; a buy order also gives up `spent` escrow.
async fn fill(txn: &ItemTxn<'_>, fx: &mut Effects, m: &Match, n: i64, spent: Option<f64>) -> Result<(), Refusal> {
    let remaining = m.remaining - n;
    let escrow = spent.map_or(m.escrow, |s| (m.escrow - s).max(0.0));
    let state = if remaining == 0 { ORDER_DONE } else { ORDER_OPEN };
    txn.tx()
        .execute(
            r#"UPDATE market_orders SET "volRemaining" = $2, "orderState" = $3, "escrow" = $4
                WHERE "orderID" = $1"#,
            &[&(m.id as i32), &(remaining as i32), &state, &escrow],
        )
        .await?;
    if m.owner >= FIRST_PLAYER_ID {
        fx.orders.push((m.id, if remaining == 0 { "Expiry" } else { "Modify" }));
    }
    Ok(())
}

async fn list(txn: &ItemTxn<'_>, me: i64, order: &NewOrder, at: &Where, quantity: i64, escrow: f64) -> Result<i64, Refusal> {
    let range = if order.bid { order.range } else { RANGE_STATION };
    let r = txn
        .tx()
        .query_one(
            r#"INSERT INTO market_orders
                   ("typeID","charID","regionID","stationID","range","bid","price","volEntered",
                    "volRemaining","issued","orderState","minVolume","contraband","accountID",
                    "duration","isCorp","solarSystemID","escrow","jumps")
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$8,$9,0,$10,0,1000,$11,0,$12,$13,1)
               RETURNING "orderID""#,
            &[
                &(order.type_id as i32),
                &(me as i32),
                &(at.region as i32),
                &(order.station as i32),
                &(range as i32),
                &i16::from(order.bid),
                &order.price,
                &(quantity as i32),
                &crate::bulkdata::filetime_now(),
                &(order.min_volume as i32),
                &(order.duration as i32),
                &(at.solar_system as i32),
                &escrow,
            ],
        )
        .await?;
    Ok(r.get::<_, i32>(0) as i64)
}

/// The broker's commission for listing an order worth `value`.
async fn broker_fee(txn: &ItemTxn<'_>, fx: &mut Effects, me: i64, value: f64) -> Result<(), Refusal> {
    let level = crate::svc::skills::levels(txn.tx(), me, &[SKILL_BROKER_RELATIONS]).await?;
    let rate = BROKER_FEE * (1.0 - 0.05 * level[&SKILL_BROKER_RELATIONS] as f64);
    let fee = (value * rate).max(MINIMUM_FEE);
    let entry = Entry { ref_type: REF_BROKER_FEE, payer: me, payee: 0, arg_id1: "", reason: "" };
    fx.balances.insert(me, post(txn.tx(), me, -fee, &entry).await?);
    Ok(())
}

/// Sales tax on `gross` earned by `seller`.
async fn pay_tax(txn: &ItemTxn<'_>, fx: &mut Effects, seller: i64, gross: f64) -> Result<(), Refusal> {
    let level = crate::svc::skills::levels(txn.tx(), seller, &[SKILL_ACCOUNTING]).await?;
    let tax = gross * TRANSACTION_TAX * (1.0 - 0.1 * level[&SKILL_ACCOUNTING] as f64);
    if tax >= 0.01 {
        let entry = Entry { ref_type: REF_TRANSACTION_TAX, payer: seller, payee: 0, arg_id1: "", reason: "" };
        fx.balances.insert(seller, post(txn.tx(), seller, -tax, &entry).await?);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn record(
    txn: &ItemTxn<'_>,
    who: i64,
    other: i64,
    type_id: i64,
    quantity: i64,
    price: f64,
    kind: i32,
    station: i64,
    region: i64,
) -> Result<(), Refusal> {
    txn.tx()
        .execute(
            r#"INSERT INTO market_transactions
                   ("transactionDateTime","typeID","quantity","price","transactionType","clientID",
                    "regionID","stationID","characterID","isCorp","accountKey")
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,0,1000)"#,
            &[
                &crate::bulkdata::filetime_now(),
                &(type_id as i32),
                &(quantity as i32),
                &price,
                &kind,
                &(other as i32),
                &(region as i32),
                &(station as i32),
                &(who as i32),
            ],
        )
        .await?;
    Ok(())
}

/// An open order of the caller's, locked.
async fn own_order(txn: &ItemTxn<'_>, me: i64, order_id: i64) -> Result<(Match, bool, i64), Refusal> {
    let r = txn
        .tx()
        .query_opt(
            r#"SELECT "orderID","charID","stationID","price","volRemaining","escrow","bid","typeID"
                 FROM market_orders WHERE "orderID" = $1 AND "charID" = $2 AND "orderState" = 0
                  FOR UPDATE"#,
            &[&(order_id as i32), &(me as i32)],
        )
        .await?;
    let Some(r) = r else { return refuse("That order is no longer open.") };
    Ok((as_match(&r), r.get::<_, i16>(6) != 0, r.get::<_, i32>(7) as i64))
}

/// Cancel: a buy order's escrow comes back, a sell order's goods go back to
/// the hangar where it was listed.
async fn cancel(txn: &mut ItemTxn<'_>, fx: &mut Effects, me: i64, order_id: i64) -> Result<(), Refusal> {
    let (order, bid, type_id) = own_order(txn, me, order_id).await?;
    if bid {
        if order.escrow > 0.0 {
            let entry = Entry { ref_type: REF_MARKET_ESCROW, payer: 0, payee: me, arg_id1: "", reason: "" };
            fx.balances.insert(me, post(txn.tx(), me, order.escrow, &entry).await?);
        }
    } else if order.remaining > 0 {
        let stack = NewItem { type_id, owner_id: me, location_id: order.station, flag: FLAG_HANGAR.into(), quantity: order.remaining, singleton: false };
        txn.stack_onto(stack).await?;
    }
    txn.tx()
        .execute(
            r#"UPDATE market_orders SET "orderState" = $2, "escrow" = 0 WHERE "orderID" = $1"#,
            &[&(order_id as i32), &ORDER_CANCELLED],
        )
        .await?;
    fx.orders.push((order_id, "Expiry"));
    info!("cancelled order {order_id}");
    Ok(())
}

/// Reprice: a buy order's escrow follows its price.
async fn modify(txn: &mut ItemTxn<'_>, fx: &mut Effects, me: i64, order_id: i64, price: f64) -> Result<(), Refusal> {
    if !price.is_finite() || price <= 0.0 {
        return refuse("Enter a price.");
    }
    let (order, bid, _) = own_order(txn, me, order_id).await?;
    let mut escrow = order.escrow;
    if bid {
        let wanted = price * order.remaining as f64;
        let delta = wanted - order.escrow;
        if delta.abs() >= 0.01 {
            let entry = Entry { ref_type: REF_MARKET_ESCROW, payer: me, payee: 0, arg_id1: "", reason: "" };
            fx.balances.insert(me, post(txn.tx(), me, -delta, &entry).await?);
        }
        escrow = wanted;
    }
    txn.tx()
        .execute(
            r#"UPDATE market_orders SET "price" = $2, "escrow" = $3, "issued" = $4 WHERE "orderID" = $1"#,
            &[&(order_id as i32), &price, &escrow, &crate::bulkdata::filetime_now()],
        )
        .await?;
    fx.orders.push((order_id, "Modify"));
    info!("repriced order {order_id} to {price}");
    Ok(())
}
