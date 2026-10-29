//! Item changes made in one database transaction, and the `OnItemChange`
//! events that tell the client about them.
//!
//! Every item an operation touches is locked and snapshotted the first time it
//! is read; on commit each one is compared with how it ended up, so an
//! operation only says what to do and the events follow from the data
//! (invCache.py:111 reads them: an item at quantity 0 is gone, one whose old
//! location is 0 is new).

use std::collections::HashMap;

use eve_marshal::Value;
use tokio_postgres::{Client, Transaction};

use crate::svc::inventory::{invitem_cells, item_change, INVITEM_SELECT, IX_LOCATION_ID, IX_QUANTITY};

pub type DbResult<T> = Result<T, tokio_postgres::Error>;

/// What an operation needs to know about an item.
#[derive(Debug, Clone)]
pub struct Item {
    pub id: i64,
    pub type_id: i64,
    pub owner_id: i64,
    pub location_id: i64,
    pub flag: i64,
    pub singleton: bool,
    pub quantity: i64,
    pub category_id: i64,
    /// Volume of one unit.
    pub volume: f64,
}

/// The item row columns compared on commit: itemID up to quantity.
const COMPARED_COLUMNS: usize = 8;

pub struct ItemTxn<'c> {
    tx: Transaction<'c>,
    /// Each touched item as it was before this transaction; None if it is new.
    before: HashMap<i64, Option<Vec<Value>>>,
    order: Vec<i64>,
}

impl<'c> ItemTxn<'c> {
    pub async fn begin(client: &'c mut Client) -> DbResult<Self> {
        Ok(ItemTxn { tx: client.transaction().await?, before: HashMap::new(), order: Vec::new() })
    }

    /// The transaction, for writes that are not item changes (wallets, orders).
    pub fn tx(&self) -> &Transaction<'c> {
        &self.tx
    }

    /// Lock `id` and read it; the first read is the snapshot.
    pub async fn item(&mut self, id: i64) -> DbResult<Option<Item>> {
        let sql = format!(
            r#"{} e."itemID" = $1 FOR UPDATE OF e"#,
            INVITEM_SELECT.replace(r#"e."customInfo""#, r#"e."customInfo", t."volume""#)
        );
        let Some(r) = self.tx.query_opt(&sql, &[&(id as i32)]).await? else { return Ok(None) };
        if let std::collections::hash_map::Entry::Vacant(slot) = self.before.entry(id) {
            slot.insert(Some(invitem_cells(&r)));
            self.order.push(id);
        }
        let int = |i: usize| r.get::<_, i32>(i) as i64;
        Ok(Some(Item {
            id,
            type_id: int(1),
            owner_id: int(2),
            location_id: int(3),
            flag: int(4),
            singleton: int(6) != 0,
            quantity: int(7),
            category_id: int(9),
            volume: r.get::<_, Option<f64>>(11).unwrap_or(0.0),
        }))
    }

    /// Lock and read several items; missing ones are left out.
    pub async fn items(&mut self, ids: &[i64]) -> DbResult<Vec<Item>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.extend(self.item(*id).await?);
        }
        Ok(out)
    }

    async fn touch(&mut self, id: i64) -> DbResult<()> {
        if !self.before.contains_key(&id) {
            self.item(id).await?;
        }
        Ok(())
    }

    pub async fn move_to(&mut self, id: i64, location_id: i64, flag: i64) -> DbResult<()> {
        self.touch(id).await?;
        self.tx
            .execute(
                r#"UPDATE entity SET "locationID" = $2, "flag" = $3 WHERE "itemID" = $1"#,
                &[&(id as i32), &(location_id as i32), &(flag as i32)],
            )
            .await?;
        Ok(())
    }

    /// Set a stack's size; 0 destroys the item.
    pub async fn set_quantity(&mut self, id: i64, quantity: i64) -> DbResult<()> {
        if quantity <= 0 {
            return self.destroy(id).await;
        }
        self.touch(id).await?;
        self.tx
            .execute(r#"UPDATE entity SET "quantity" = $2 WHERE "itemID" = $1"#, &[&(id as i32), &(quantity as i32)])
            .await?;
        Ok(())
    }

    pub async fn set_owner(&mut self, id: i64, owner_id: i64) -> DbResult<()> {
        self.touch(id).await?;
        self.tx
            .execute(r#"UPDATE entity SET "ownerID" = $2 WHERE "itemID" = $1"#, &[&(id as i32), &(owner_id as i32)])
            .await?;
        Ok(())
    }

    /// Where an item floats in space (not compared on commit: the client
    /// learns positions from destiny).
    pub async fn set_position(&mut self, id: i64, pos: (f64, f64, f64)) -> DbResult<()> {
        self.tx
            .execute(r#"UPDATE entity SET "x" = $2, "y" = $3, "z" = $4 WHERE "itemID" = $1"#, &[&(id as i32), &pos.0, &pos.1, &pos.2])
            .await?;
        Ok(())
    }

    /// Where every item touched so far was before this transaction.
    pub fn origins(&self) -> Vec<i64> {
        let mut out: Vec<i64> = self
            .before
            .values()
            .flatten()
            .filter_map(|cells| cells.get(IX_LOCATION_ID as usize).and_then(|v| v.as_i64()))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    pub async fn set_singleton(&mut self, id: i64, singleton: bool) -> DbResult<()> {
        self.touch(id).await?;
        self.tx
            .execute(
                r#"UPDATE entity SET "singleton" = $2 WHERE "itemID" = $1"#,
                &[&(id as i32), &i32::from(singleton)],
            )
            .await?;
        Ok(())
    }

    pub async fn destroy(&mut self, id: i64) -> DbResult<()> {
        self.touch(id).await?;
        self.tx.execute(r#"DELETE FROM entity_attributes WHERE "itemID" = $1"#, &[&(id as i32)]).await?;
        self.tx.execute(r#"DELETE FROM entity WHERE "itemID" = $1"#, &[&(id as i32)]).await?;
        Ok(())
    }

    /// A new item; returns its ID.
    pub async fn create(&mut self, new: NewItem) -> DbResult<i64> {
        let r = self
            .tx
            .query_one(
                r#"INSERT INTO entity ("itemID","itemName","typeID","ownerID","locationID","flag",
                                       "contraband","singleton","quantity","x","y","z","customInfo")
                   VALUES (nextval('dyn_id_seq'), '', $1, $2, $3, $4, 0, $5, $6, 0, 0, 0, NULL)
                   RETURNING "itemID""#,
                &[
                    &(new.type_id as i32),
                    &(new.owner_id as i32),
                    &(new.location_id as i32),
                    &(new.flag as i32),
                    &i32::from(new.singleton),
                    &(new.quantity as i32),
                ],
            )
            .await?;
        let id = r.get::<_, i32>(0) as i64;
        self.before.insert(id, None);
        self.order.push(id);
        Ok(id)
    }

    /// Add `quantity` of a type to a stack at (owner, location, flag), making
    /// one if there is none. Returns the stack's ID.
    pub async fn stack_onto(&mut self, new: NewItem) -> DbResult<i64> {
        let existing = self
            .tx
            .query_opt(
                r#"SELECT "itemID" FROM entity
                    WHERE "ownerID" = $1 AND "locationID" = $2 AND "flag" = $3
                      AND "typeID" = $4 AND "singleton" = 0
                    ORDER BY "itemID" LIMIT 1"#,
                &[
                    &(new.owner_id as i32),
                    &(new.location_id as i32),
                    &(new.flag as i32),
                    &(new.type_id as i32),
                ],
            )
            .await?;
        match existing {
            Some(r) => {
                let id = r.get::<_, i32>(0) as i64;
                let have = self.item(id).await?.map_or(0, |i| i.quantity);
                self.set_quantity(id, have + new.quantity).await?;
                Ok(id)
            }
            None => self.create(new).await,
        }
    }

    /// Commit, and return the `OnItemChange` argument tuples for everything
    /// that changed.
    pub async fn commit(self) -> DbResult<Vec<Value>> {
        let ids: Vec<i32> = self.order.iter().map(|i| *i as i32).collect();
        let sql = format!(r#"{INVITEM_SELECT} e."itemID" = ANY($1)"#);
        let after: HashMap<i64, Vec<Value>> = self
            .tx
            .query(&sql, &[&ids])
            .await?
            .iter()
            .map(|r| (r.get::<_, i32>(0) as i64, invitem_cells(r)))
            .collect();
        self.tx.commit().await?;

        let mut events = Vec::new();
        for id in &self.order {
            let before = self.before.get(id).cloned().flatten();
            let event = match (before, after.get(id)) {
                (Some(old), Some(now)) => {
                    let changed: Vec<(i64, Value)> = (0..COMPARED_COLUMNS)
                        .filter(|&i| old[i] != now[i])
                        .map(|i| (i as i64, old[i].clone()))
                        .collect();
                    (!changed.is_empty()).then(|| item_change(now.clone(), changed))
                }
                (Some(mut old), None) => {
                    let quantity = old[IX_QUANTITY as usize].clone();
                    old[IX_QUANTITY as usize] = Value::Int(0);
                    Some(item_change(old, vec![(IX_QUANTITY, quantity)]))
                }
                (None, Some(now)) => Some(item_change(
                    now.clone(),
                    vec![(IX_LOCATION_ID, Value::Int(0)), (IX_QUANTITY, Value::Int(0))],
                )),
                (None, None) => None,
            };
            events.extend(event);
        }
        Ok(events)
    }
}

pub struct NewItem {
    pub type_id: i64,
    pub owner_id: i64,
    pub location_id: i64,
    pub flag: i64,
    pub quantity: i64,
    pub singleton: bool,
}

/// Queue `OnItemChange` events for one client.
pub fn notify(world: &crate::world::World, client_id: i64, events: Vec<Value>) {
    let world = world.lock();
    for event in events {
        world.notify(&[client_id], "OnItemChange", event);
    }
}
