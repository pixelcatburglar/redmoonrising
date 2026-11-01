//! The bots' economy: what they mine, refine, haul, build, buy and sell, and
//! the real market orders that carry it to players.
//!
//! - Miners bring ore home and sell it into their station's stock (the
//!   `botStock` ledger: the bots' goods in each station, between them, kept
//!   apart from item rows) for the bot buying price there.
//! - Every economy tick, ore in stock anywhere is refined into minerals.
//!   Haulers carry stock from where it was made to the nearest market hub
//!   (the five trade hubs and each null-sec alliance's capital), and are paid
//!   a cut of what it is worth there.
//! - At each hub a trader bot keeps real sell orders for everything in stock
//!   there, priced by how much there is (scarce goods dear, gluts cheap, always
//!   under the NPC asks), and real buy orders for ore and minerals above the
//!   NPC bids. Players buy from and sell to these like any order; what players
//!   sell into them is taken from the trader's hangar into stock, and the
//!   player buy orders the stock can fill at the bots' price are filled.
//! - Hubs build hulls out of their minerals (each hull's bill of materials),
//!   the ones bots have been short of first. Bots whose ships were lost buy
//!   new ones here (`buy_hull`), and so do bots moving up a class; with none in
//!   stock they pay the NPC price, and the shortage is remembered.
//! - Corporations take tax on their members' earnings; bot corporations pass
//!   a share of it on to their alliance's executor, which pays for towers.
//!
//! Bot-to-bot trade is written to the market's transactions, so the price
//! history players see follows the bots' market too.

use std::collections::{HashMap, HashSet, VecDeque};

use tokio_postgres::Client;
use tracing::{error, info};

use super::behaviour::{Atlas, Bot};
use crate::svc::rows::{float, int, text};

const GROUP_MINERAL: i32 = 18;
const CATEGORY_ASTEROID: i32 = 25;
/// TL2MaterialsForTypeWithActivity: manufacturing.
const ACTIVITY_MANUFACTURING: i32 = 6;
const FIRST_PLAYER_ID: i32 = 100_000_000;
const RANGE_STATION: i32 = -1;
const ORDER_OPEN: i32 = 0;
const ORDER_DONE: i32 = 2;
const ORDER_DAYS: i32 = 90;
const TRANSACTION_SALE: i32 = 0;
const TRANSACTION_PURCHASE: i32 = 1;
const REF_MARKET_TRANSACTION: i32 = 2;
const FLAG_HANGAR: i32 = 4;

/// What refining keeps of the minerals in ore.
const REFINE_YIELD: f64 = 0.8;
/// What miners get for their ore: this share of the bots' buying price.
const MINER_CUT: f64 = 0.95;
/// What a hauler earns: this share of the value it delivers.
const HAULER_CUT: f64 = 0.04;
/// Stock worth this much (ore and minerals) at a hub is "enough": prices sit
/// halfway between their dearest and cheapest.
const ENOUGH_VALUE: f64 = 40_000_000.0;
/// Hulls held at a hub before they count as plenty.
const ENOUGH_HULLS: f64 = 12.0;
/// Hulls a hub can build each economy tick.
const BUILDS_PER_TICK: usize = 4;
/// Bot corporations' tax on members' earnings, and the share of it their
/// alliance takes.
pub const BOT_CORP_TAX: f64 = 0.1;
const ALLIANCE_SHARE: f64 = 0.2;
/// The most of its wallet a trader puts into buy orders.
const ESCROW_SHARE: f64 = 0.3;
/// How long the shortage of a hull is remembered: kept this share each tick.
const DEMAND_KEPT: f64 = 0.9;

/// The security bands ore is found in, and what each holds.
const ORES: [&[i32]; 4] = [
    // 0.7 and up: Veldspar, Scordite.
    &[1230, 1228],
    // High-sec below 0.7: and Pyroxeres, Plagioclase, Omber.
    &[1230, 1228, 1224, 18, 1227],
    // Low-sec: Kernite, Jaspet, Hemorphite, Hedbergite, Omber.
    &[20, 1226, 1231, 21, 1227],
    // Null-sec: Gneiss, Dark Ochre, Spodumain, Crokite, Bistot, Arkonor.
    &[1229, 1232, 19, 1225, 1223, 22],
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ore,
    Mineral,
    Hull,
}

#[derive(Debug, Clone)]
pub struct Good {
    pub name: String,
    pub kind: Kind,
    /// m3 a unit.
    pub volume: f64,
    /// The NPC market's best ask and bid: the bots sell below the one and
    /// buy above the other.
    pub ask: f64,
    pub bid: f64,
}

/// One of the bots' standing orders at a hub.
#[derive(Debug, Clone)]
struct Order {
    id: i32,
    /// What it was left at, to tell what players took since.
    listed: i64,
    escrow: f64,
}

/// What the economy did since the server started, for the dashboard.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub mined_m3: f64,
    pub refined_units: i64,
    pub hauled_m3: f64,
    pub hulls_built: i64,
    pub hulls_bought: i64,
    pub hulls_bought_from_npcs: i64,
    pub sold_to_players: f64,
    pub bought_from_players: f64,
    pub player_orders_filled: i64,
    pub orders: usize,
    pub corp_tax: f64,
}

pub struct Economy {
    pub goods: HashMap<i32, Good>,
    /// Ore: units a batch, and the minerals a batch gives.
    refine: HashMap<i32, (i64, Vec<(i32, i64)>)>,
    /// Hulls: the minerals one takes.
    bom: HashMap<i32, Vec<(i32, i64)>>,
    stock: HashMap<(i32, i32), i64>,
    dirty: HashSet<(i32, i32)>,
    pub hubs: Vec<i32>,
    hub_by_system: HashMap<i32, i32>,
    station_system: HashMap<i32, i32>,
    region_of_system: HashMap<i32, i32>,
    /// Hulls bots wanted and could not get from stock.
    demand: HashMap<i32, f64>,
    orders: HashMap<(i32, i32, bool), Order>,
    /// Each hub's trader: the bot whose orders are the bots' there.
    pub traders: HashMap<i32, u32>,
    trader_ids: HashSet<i32>,
    /// ISK the economy moved to (or from) bots other than the one stepping:
    /// the sim applies them.
    pub payments: Vec<(u32, f64)>,
    /// Bot-to-bot trade since the last tick, by (station, type): (units, ISK).
    trades: HashMap<(i32, i32), (i64, f64)>,
    /// Corporation income to post: tax collected, by corporation.
    pub corp_income: HashMap<i32, f64>,
    /// Player corporations' tax rates (bot corporations take `BOT_CORP_TAX`).
    pub tax_rates: HashMap<i32, f64>,
    /// Each bot corporation's alliance executor, for its share of tax.
    pub executor_of: HashMap<i32, i32>,
    pub stats: Stats,
    /// Stations outside the hubs with goods worth a hauler's trip: (station,
    /// system), refreshed each tick.
    sources: Vec<(i32, i32)>,
    /// Market reports for the dashboard: hub, type, stock, ask.
    pub board: Vec<(i32, i32, i64, f64)>,
    pub log: VecDeque<String>,
}

impl Economy {
    /// An economy with nothing in it, for tests.
    pub fn empty() -> Economy {
        Economy {
            goods: HashMap::new(),
            refine: HashMap::new(),
            bom: HashMap::new(),
            stock: HashMap::new(),
            dirty: HashSet::new(),
            hubs: Vec::new(),
            hub_by_system: HashMap::new(),
            station_system: HashMap::new(),
            region_of_system: HashMap::new(),
            demand: HashMap::new(),
            orders: HashMap::new(),
            traders: HashMap::new(),
            trader_ids: HashSet::new(),
            payments: Vec::new(),
            trades: HashMap::new(),
            corp_income: HashMap::new(),
            tax_rates: HashMap::new(),
            executor_of: HashMap::new(),
            stats: Stats::default(),
            sources: Vec::new(),
            board: Vec::new(),
            log: VecDeque::new(),
        }
    }

    // ---- prices ----------------------------------------------------------------------------

    fn stock_at(&self, station: i32, type_id: i32) -> i64 {
        self.stock.get(&(station, type_id)).copied().unwrap_or(0)
    }

    fn add(&mut self, station: i32, type_id: i32, n: i64) {
        if n == 0 {
            return;
        }
        let e = self.stock.entry((station, type_id)).or_insert(0);
        *e = (*e + n).max(0);
        self.dirty.insert((station, type_id));
    }

    /// How short a hub is of a good: 1 with none, towards 0 with plenty.
    fn scarcity(&self, hub: i32, type_id: i32) -> f64 {
        let Some(g) = self.goods.get(&type_id) else { return 0.5 };
        let have = self.stock_at(hub, type_id) as f64;
        let enough = match g.kind {
            Kind::Hull => ENOUGH_HULLS + self.demand.get(&type_id).copied().unwrap_or(0.0),
            _ => ENOUGH_VALUE / g.bid.max(0.01),
        };
        1.0 / (1.0 + have / enough.max(1.0))
    }

    /// What the bots sell a good for at a hub: between just over the NPC bid
    /// (a glut) and just under the NPC ask (none at all).
    pub fn ask(&self, hub: i32, type_id: i32) -> f64 {
        let Some(g) = self.goods.get(&type_id) else { return 0.0 };
        let (lo, hi) = (g.bid * 1.05, (g.ask * 0.95).max(g.bid * 1.1));
        round_price(lo + (hi - lo) * self.scarcity(hub, type_id))
    }

    /// What the bots pay for ore and minerals at a hub: above the NPC bid,
    /// well under their own ask.
    pub fn bid(&self, hub: i32, type_id: i32) -> f64 {
        let Some(g) = self.goods.get(&type_id) else { return 0.0 };
        round_price((self.ask(hub, type_id) * 0.75).max(g.bid * 1.02))
    }

    /// The hub a station's goods go to.
    pub fn hub_for(&self, station: i32) -> Option<i32> {
        if self.hubs.contains(&station) {
            return Some(station);
        }
        let system = self.station_system.get(&station)?;
        self.hub_by_system.get(system).copied()
    }

    pub fn is_hub(&self, station: i32) -> bool {
        self.hubs.contains(&station)
    }

    /// Whether a bot keeps a hub's orders (and so stays at its hub).
    pub fn is_trader(&self, char_id: i32) -> bool {
        self.trader_ids.contains(&char_id)
    }

    // ---- what bots do ------------------------------------------------------------------------

    /// The ore a belt in a system of this security gives, picked by `roll`.
    pub fn ore_for(&self, security: f32, roll: f64) -> Option<i32> {
        let band = if security >= 0.7 {
            0
        } else if security >= 0.45 {
            1
        } else if security > 0.0 {
            2
        } else {
            3
        };
        let list: Vec<i32> = ORES[band].iter().copied().filter(|o| self.goods.contains_key(o)).collect();
        list.get(((roll * list.len() as f64) as usize).min(list.len().saturating_sub(1))).copied()
    }

    pub fn volume(&self, type_id: i32) -> f64 {
        self.goods.get(&type_id).map_or(1.0, |g| g.volume.max(0.0001))
    }

    /// A miner home with ore sells it into the station's stock; returns
    /// what it earned.
    pub fn sell_ore(&mut self, station: i32, hold: &mut Vec<(i32, i64)>) -> f64 {
        let hub = self.hub_for(station).unwrap_or(station);
        let mut earned = 0.0;
        for (ore, n) in hold.drain(..) {
            let price = self.bid(hub, ore) * MINER_CUT;
            earned += price * n as f64;
            self.stats.mined_m3 += n as f64 * self.goods.get(&ore).map_or(0.0, |g| g.volume);
            self.add(station, ore, n);
        }
        earned
    }

    /// The goods worth hauling from a station, most valuable for their
    /// bulk first, up to `capacity` m3; taken out of its stock.
    pub fn load(&mut self, station: i32, capacity: f64) -> Vec<(i32, i64)> {
        if self.is_hub(station) || capacity <= 0.0 {
            return Vec::new();
        }
        let mut there: Vec<(i32, i64, f64)> = self
            .stock
            .iter()
            .filter(|((s, _), n)| *s == station && **n > 0)
            .filter_map(|((_, t), n)| self.goods.get(t).filter(|g| g.kind != Kind::Ore).map(|g| (*t, *n, g.bid / g.volume.max(0.0001))))
            .collect();
        there.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(&b.0)));
        let mut room = capacity;
        let mut out = Vec::new();
        for (t, n, _) in there {
            let v = self.volume(t);
            let take = n.min((room / v).floor() as i64);
            if take <= 0 {
                continue;
            }
            room -= take as f64 * v;
            self.add(station, t, -take);
            out.push((t, take));
        }
        out
    }

    /// A hauler at a hub hands over its load; returns its pay.
    pub fn unload(&mut self, hub: i32, hold: &mut Vec<(i32, i64)>) -> f64 {
        let mut value = 0.0;
        for (t, n) in hold.drain(..) {
            value += self.ask(hub, t) * n as f64;
            self.stats.hauled_m3 += n as f64 * self.volume(t);
            self.add(hub, t, n);
        }
        value * HAULER_CUT
    }

    /// The stations near `system` with the most worth hauling, for a hauler
    /// with nothing to carry: the best within `jumps`, if any is worth a trip.
    pub fn source_near(&self, atlas: &Atlas, system: i32, jumps: i64) -> Option<i32> {
        if self.sources.is_empty() {
            return None;
        }
        let near: HashMap<i32, i64> = crate::gates::reach_on(atlas.graph, system as i64, jumps).into_iter().map(|(s, n)| (s as i32, n)).collect();
        // The nearest; the list is richest first, so ties go to the richer.
        self.sources.iter().filter_map(|(station, sys)| near.get(sys).map(|d| (*d, *station))).min_by_key(|(d, _)| *d).map(|(_, s)| s)
    }

    /// Where there is stock worth hauling, richest first.
    fn find_sources(&mut self) {
        let mut worth: HashMap<i32, f64> = HashMap::new();
        for ((station, t), n) in &self.stock {
            if *n <= 0 || self.is_hub(*station) {
                continue;
            }
            if let Some(g) = self.goods.get(t).filter(|g| g.kind != Kind::Ore) {
                *worth.entry(*station).or_default() += g.bid * *n as f64;
            }
        }
        let mut list: Vec<(i32, f64)> = worth.into_iter().filter(|(_, v)| *v > 2_000_000.0).collect();
        list.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        self.sources = list.into_iter().filter_map(|(s, _)| self.station_system.get(&s).map(|sys| (s, *sys))).collect();
    }

    /// A bot buys a hull near `station`: from the nearest hub's stock, or
    /// any hub's, or at the NPC price. Returns the price, or None if it
    /// cannot pay that much (`budget`).
    pub fn buy_hull(&mut self, hull: i32, station: i32, budget: f64, list_price: f64) -> Option<f64> {
        let near = self.hub_for(station);
        let from = near
            .filter(|h| self.stock_at(*h, hull) > 0)
            .or_else(|| self.hubs.iter().copied().filter(|h| self.stock_at(*h, hull) > 0).max_by_key(|h| self.stock_at(*h, hull)));
        match from {
            Some(hub) => {
                let price = self.ask(hub, hull);
                if price > budget {
                    return None;
                }
                self.add(hub, hull, -1);
                if let Some(&trader) = self.traders.get(&hub) {
                    self.payments.push((trader, price));
                }
                let t = self.trades.entry((hub, hull)).or_default();
                t.0 += 1;
                t.1 += price;
                self.stats.hulls_bought += 1;
                Some(price)
            }
            None => {
                let price = self.goods.get(&hull).map_or(list_price, |g| g.ask);
                *self.demand.entry(hull).or_default() += 1.0;
                if price > budget {
                    return None;
                }
                self.stats.hulls_bought_from_npcs += 1;
                Some(price)
            }
        }
    }

    /// Tax on what a bot earned, for its corporation; returns the tax.
    pub fn tax(&mut self, corp: i32, earned: f64, bot_corp: bool) -> f64 {
        let rate = if bot_corp { BOT_CORP_TAX } else { self.tax_rates.get(&corp).copied().unwrap_or(0.0) };
        let tax = earned * rate.clamp(0.0, 1.0);
        if tax >= 1.0 {
            match self.executor_of.get(&corp).filter(|e| **e != corp) {
                Some(&exec) => {
                    *self.corp_income.entry(corp).or_default() += tax * (1.0 - ALLIANCE_SHARE);
                    *self.corp_income.entry(exec).or_default() += tax * ALLIANCE_SHARE;
                }
                None => *self.corp_income.entry(corp).or_default() += tax,
            }
            self.stats.corp_tax += tax;
        }
        tax
    }

    /// Hulls a hub can build, for the news of what is in short supply.
    pub fn shortages(&self) -> Vec<(i32, f64)> {
        let mut v: Vec<(i32, f64)> = self.demand.iter().filter(|(_, d)| **d >= 1.0).map(|(t, d)| (*t, *d)).collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1));
        v
    }

    fn note(&mut self, line: String) {
        self.log.push_back(line);
        while self.log.len() > 30 {
            self.log.pop_front();
        }
    }

    // ---- ticks (no database) -----------------------------------------------------------------

    /// Ore anywhere in stock becomes minerals.
    pub fn refine_all(&mut self) {
        self.refine_ore();
        self.find_sources();
    }

    fn refine_ore(&mut self) {
        let ores: Vec<((i32, i32), i64)> =
            self.stock.iter().filter(|((_, t), n)| **n > 0 && self.refine.contains_key(t)).map(|(k, n)| (*k, *n)).collect();
        for ((station, ore), n) in ores {
            let (portion, minerals) = self.refine[&ore].clone();
            let batches = n / portion.max(1);
            if batches == 0 {
                continue;
            }
            self.add(station, ore, -batches * portion);
            for (m, per) in minerals {
                let out = (batches as f64 * per as f64 * REFINE_YIELD) as i64;
                self.add(station, m, out);
                self.stats.refined_units += out;
            }
        }
    }

    /// Each hub builds what it can of the hulls bots are short of, then of
    /// those it holds few of.
    pub fn build(&mut self, roll: &mut impl FnMut() -> f64) {
        let mut wanted: Vec<(i32, f64)> = self.demand.iter().map(|(t, d)| (*t, *d)).filter(|(_, d)| *d > 0.0).collect();
        wanted.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let catalogue: Vec<i32> = {
            let mut c: Vec<i32> = self.bom.keys().copied().collect();
            c.sort_unstable();
            c
        };
        for hub in self.hubs.clone() {
            let mut built = 0;
            let mut tries = 0;
            while built < BUILDS_PER_TICK && tries < BUILDS_PER_TICK * 4 {
                tries += 1;
                let pick = wanted
                    .iter()
                    .find(|(t, d)| *d >= 1.0 && self.can_build(hub, *t))
                    .map(|(t, _)| *t)
                    .or_else(|| {
                        let t = *catalogue.get((roll() * catalogue.len() as f64) as usize)?;
                        (self.stock_at(hub, t) < 3 && self.can_build(hub, t)).then_some(t)
                    });
                let Some(hull) = pick else { continue };
                for (m, n) in self.bom[&hull].clone() {
                    self.add(hub, m, -n);
                }
                self.add(hub, hull, 1);
                if let Some(d) = self.demand.get_mut(&hull) {
                    *d = (*d - 1.0).max(0.0);
                }
                if let Some(w) = wanted.iter_mut().find(|(t, _)| *t == hull) {
                    w.1 -= 1.0;
                }
                self.stats.hulls_built += 1;
                built += 1;
            }
        }
        for d in self.demand.values_mut() {
            *d *= DEMAND_KEPT;
        }
        self.demand.retain(|_, d| *d > 0.05);
    }

    fn can_build(&self, hub: i32, hull: i32) -> bool {
        self.bom.get(&hull).is_some_and(|b| !b.is_empty() && b.iter().all(|(m, n)| self.stock_at(hub, *m) >= *n))
    }
}

fn round_price(p: f64) -> f64 {
    if p >= 100.0 {
        // Four significant figures, as players price.
        let scale = 10f64.powi(p.log10().floor() as i32 - 3);
        (p / scale).round() * scale
    } else {
        (p * 100.0).round() / 100.0
    }
}

// ---- loading -------------------------------------------------------------------------------

/// Reads the goods, refining and bills of materials, the NPC prices, and
/// what the bots already hold.
pub async fn load(db: &Client, atlas: &Atlas, hull_types: &[i32]) -> Result<Economy, tokio_postgres::Error> {
    let mut e = Economy::empty();
    let ores: Vec<i32> = ORES.iter().flat_map(|b| b.iter().copied()).collect();
    let sql = r#"SELECT t."typeID", t."typeName", COALESCE(t."volume", 1)::float8, g."categoryID", t."groupID",
                        MIN(o."price") FILTER (WHERE o."bid" = 0), MAX(o."price") FILTER (WHERE o."bid" = 1)
                   FROM "invTypes" t
                   JOIN "invGroups" g ON g."groupID" = t."groupID"
                   LEFT JOIN market_orders o ON o."typeID" = t."typeID" AND o."charID" < $3 AND o."orderState" = 0
                  WHERE t."typeID" = ANY($1) OR t."groupID" = $2
                  GROUP BY t."typeID", t."typeName", t."volume", g."categoryID", t."groupID""#;
    let wanted: Vec<i32> = ores.iter().copied().chain(hull_types.iter().copied()).collect();
    for r in db.query(sql, &[&wanted, &GROUP_MINERAL, &FIRST_PLAYER_ID]).await? {
        let type_id = int(&r, 0).unwrap_or(0) as i32;
        let (Some(ask), Some(bid)) = (float(&r, 5), float(&r, 6)) else { continue };
        let kind = if int(&r, 3) == Some(CATEGORY_ASTEROID as i64) {
            Kind::Ore
        } else if int(&r, 4) == Some(GROUP_MINERAL as i64) {
            Kind::Mineral
        } else {
            Kind::Hull
        };
        let volume = if kind == Kind::Hull { 2_500.0 } else { float(&r, 2).unwrap_or(1.0) };
        e.goods.insert(type_id, Good { name: text(&r, 1).unwrap_or_default(), kind, volume, ask, bid: bid.min(ask) });
    }
    let sql = r#"SELECT r."oreTypeID", r."mineralTypeID", r."amountPerBatch", COALESCE(t."portionSize", 1)
                   FROM "invOreReprocessing" r JOIN "invTypes" t ON t."typeID" = r."oreTypeID"
                  WHERE r."oreTypeID" = ANY($1)"#;
    for r in db.query(sql, &[&ores]).await? {
        let (ore, mineral, amount, portion) = (int(&r, 0).unwrap_or(0) as i32, int(&r, 1).unwrap_or(0) as i32, int(&r, 2).unwrap_or(0), int(&r, 3).unwrap_or(1));
        if !e.goods.contains_key(&mineral) {
            continue;
        }
        let entry = e.refine.entry(ore).or_insert((portion.max(1), Vec::new()));
        entry.1.push((mineral, amount));
    }
    let sql = r#"SELECT m."typeID", m."requiredTypeID", m."quantity" FROM "TL2MaterialsForTypeWithActivity" m
                   JOIN "invTypes" t ON t."typeID" = m."requiredTypeID"
                  WHERE m."activity" = $2 AND m."damagePerJob" = 1 AND t."groupID" = $3 AND m."typeID" = ANY($1)"#;
    for r in db.query(sql, &[&hull_types, &ACTIVITY_MANUFACTURING, &GROUP_MINERAL]).await? {
        e.bom.entry(int(&r, 0).unwrap_or(0) as i32).or_default().push((int(&r, 1).unwrap_or(0) as i32, int(&r, 2).unwrap_or(0)));
    }
    e.bom.retain(|h, _| e.goods.contains_key(h));
    for r in db.query(r#"SELECT "stationID", "typeID", "quantity" FROM "botStock" WHERE "quantity" > 0"#, &[]).await? {
        e.stock.insert((int(&r, 0).unwrap_or(0) as i32, int(&r, 1).unwrap_or(0) as i32), int(&r, 2).unwrap_or(0));
    }
    e.station_system = atlas.station_system.clone();
    e.region_of_system = atlas.systems.iter().map(|(id, s)| (*id, s.region)).collect();
    info!(
        "bots: economy of {} goods ({} ores refine, {} hulls buildable), {} stock lines",
        e.goods.len(),
        e.refine.len(),
        e.bom.len(),
        e.stock.len()
    );
    Ok(e)
}

/// The markets: the trade hubs and the null-sec capitals; each system goes
/// to its nearest by jumps. The traders: at each hub a trader living there,
/// else any bot living there.
pub fn set_hubs(e: &mut Economy, atlas: &Atlas, hubs: Vec<i32>, bots: &[Bot]) {
    let mut hubs: Vec<i32> = hubs.into_iter().filter(|h| atlas.station_system.contains_key(h)).collect();
    hubs.sort_unstable();
    hubs.dedup();
    let mut nearest: HashMap<i32, i32> = HashMap::new();
    let mut queue: VecDeque<i32> = VecDeque::new();
    for h in &hubs {
        let system = atlas.station_system[h];
        if nearest.insert(system, *h).is_none() {
            queue.push_back(system);
        }
    }
    while let Some(s) = queue.pop_front() {
        let hub = nearest[&s];
        for n in atlas.graph.get(&(s as i64)).into_iter().flatten() {
            let n = *n as i32;
            if let std::collections::hash_map::Entry::Vacant(v) = nearest.entry(n) {
                v.insert(hub);
                queue.push_back(n);
            }
        }
    }
    e.hub_by_system = nearest;
    e.traders.clear();
    for h in &hubs {
        let living: Vec<usize> = (0..bots.len()).filter(|i| bots[*i].home == *h).collect();
        let trader = living.iter().find(|i| bots[**i].role == super::behaviour::Role::Trader).or(living.first());
        if let Some(&i) = trader {
            e.traders.insert(*h, i as u32);
        }
    }
    e.trader_ids = e.traders.values().map(|i| bots[*i as usize].id).collect();
    e.hubs = hubs;
    e.find_sources();
}

// ---- writing it down -----------------------------------------------------------------------

/// Saves the stock that changed and posts corporation income.
pub async fn flush(e: &mut Economy, db: &Client) -> Result<(), tokio_postgres::Error> {
    if !e.dirty.is_empty() {
        let keys: Vec<(i32, i32)> = e.dirty.drain().collect();
        let stations: Vec<i32> = keys.iter().map(|k| k.0).collect();
        let types: Vec<i32> = keys.iter().map(|k| k.1).collect();
        let quantities: Vec<i64> = keys.iter().map(|k| e.stock.get(k).copied().unwrap_or(0)).collect();
        db.execute(
            r#"INSERT INTO "botStock" ("stationID", "typeID", "quantity")
               SELECT * FROM unnest($1::int[], $2::int[], $3::bigint[])
               ON CONFLICT ("stationID", "typeID") DO UPDATE SET "quantity" = EXCLUDED."quantity""#,
            &[&stations, &types, &quantities],
        )
        .await?;
        e.stock.retain(|_, n| *n > 0);
    }
    if !e.corp_income.is_empty() {
        let (corps, amounts): (Vec<i32>, Vec<f64>) = e.corp_income.drain().unzip();
        let now = crate::bulkdata::filetime_now();
        // The corporation's wallet, and a line in its journal.
        db.execute(
            r#"WITH u AS (SELECT * FROM unnest($1::int[], $2::float8[]) AS u(corp, amount)),
                    paid AS (UPDATE "corporation" co SET "balance" = co."balance" + u.amount FROM u
                              WHERE co."corporationID" = u.corp RETURNING co."corporationID", co."balance", u.amount)
               INSERT INTO market_journal ("characterID","transDate","refTypeID","ownerID1","ownerID2","argID1",
                                           "accountID","amount","balance","reason","accountKey")
               SELECT p."corporationID", $3::bigint, 10, 0, p."corporationID", '', 0, p.amount, p."balance", 'Member tax (simulated pilots)', 1000
                 FROM paid p
                WHERE NOT EXISTS (SELECT 1 FROM "botGroups" g WHERE g."ownerID" = p."corporationID")"#,
            &[&corps, &amounts, &now],
        )
        .await?;
    }
    Ok(())
}

/// The economy's slower half, every politics tick: refining, building,
/// the hubs' orders brought into line with their stock, what players
/// bought and sold through them, and the bots' own trade written into the
/// market's history. Returns the ISK the traders' orders took or gave back
/// (escrow), by bot index.
pub async fn tick(e: &mut Economy, db: &Client, db_url: &str, world: &crate::world::World, bots: &[Bot], roll: &mut impl FnMut() -> f64) -> Result<(), tokio_postgres::Error> {
    e.refine_all();
    e.build(roll);
    if e.traders.is_empty() {
        return Ok(());
    }
    let now = crate::bulkdata::filetime_now();
    reconcile(e, db, bots).await?;
    absorb(e, db, bots).await?;
    if let Err(err) = fill_player_bids(e, db, db_url, world, bots).await {
        error!("bots: filling players' buy orders: {}", crate::logging::chain(&err));
    }
    place(e, db, bots, now).await?;
    history(e, db, bots, now).await?;
    let mut board = Vec::new();
    for &hub in &e.hubs {
        for (t, g) in &e.goods {
            let have = e.stock_at(hub, *t);
            if have > 0 || g.kind != Kind::Hull {
                board.push((hub, *t, have, e.ask(hub, *t)));
            }
        }
    }
    e.board = board;
    Ok(())
}

/// What players took from the bots' orders since the last look.
async fn reconcile(e: &mut Economy, db: &Client, bots: &[Bot]) -> Result<(), tokio_postgres::Error> {
    let ids: Vec<i32> = e.orders.values().map(|o| o.id).collect();
    let rows = db.query(r#"SELECT "orderID", "volRemaining", "orderState", "escrow", "price" FROM market_orders WHERE "orderID" = ANY($1)"#, &[&ids]).await?;
    let found: HashMap<i32, (i64, i32, f64, f64)> = rows
        .iter()
        .map(|r| (int(r, 0).unwrap_or(0) as i32, (int(r, 1).unwrap_or(0), int(r, 2).unwrap_or(0) as i32, float(r, 3).unwrap_or(0.0), float(r, 4).unwrap_or(0.0))))
        .collect();
    let keys: Vec<(i32, i32, bool)> = e.orders.keys().copied().collect();
    for key in keys {
        let (hub, t, bid) = key;
        let order = e.orders[&key].clone();
        let Some(&(remaining, state, escrow, price)) = found.get(&order.id) else {
            e.orders.remove(&key);
            continue;
        };
        let taken = (order.listed - remaining).max(0);
        if taken > 0 {
            if bid {
                // Bought from players: the goods are in the trader's hangar (`absorb`).
                e.stats.bought_from_players += taken as f64 * price;
            } else {
                e.add(hub, t, -taken);
                e.stats.sold_to_players += taken as f64 * price;
                let name = e.goods.get(&t).map_or_else(|| t.to_string(), |g| g.name.clone());
                e.note(format!("Players bought {taken} {name} from the bots at {}", hub));
            }
        }
        if state != ORDER_OPEN {
            // Done (or cancelled from outside): its escrow, if any, back to the trader.
            if bid && escrow > 0.0 {
                if let Some(&trader) = e.traders.get(&hub) {
                    e.payments.push((trader, escrow));
                }
            }
            e.orders.remove(&key);
        } else if let Some(o) = e.orders.get_mut(&key) {
            o.listed = remaining;
            o.escrow = escrow;
        }
    }
    let _ = bots;
    Ok(())
}

/// What players sold into the bots' buy orders: out of the traders'
/// hangars, into stock.
async fn absorb(e: &mut Economy, db: &Client, bots: &[Bot]) -> Result<(), tokio_postgres::Error> {
    let traders: Vec<i32> = e.traders.values().map(|i| bots[*i as usize].id).collect();
    let goods: Vec<i32> = e.goods.keys().copied().collect();
    let rows = db
        .query(
            r#"DELETE FROM entity WHERE "ownerID" = ANY($1) AND "flag" = $2 AND "singleton" = 0 AND "typeID" = ANY($3)
                                    AND "locationID" = ANY($4)
               RETURNING "locationID", "typeID", "quantity""#,
            &[&traders, &FLAG_HANGAR, &goods, &e.hubs],
        )
        .await?;
    for r in rows {
        e.add(int(&r, 0).unwrap_or(0) as i32, int(&r, 1).unwrap_or(0) as i32, int(&r, 2).unwrap_or(0));
    }
    Ok(())
}

/// The orders each hub should have: a sell order for all its stock, and
/// buy orders for the ore and minerals it is short of; set up or brought up
/// to date.
async fn place(e: &mut Economy, db: &Client, bots: &[Bot], now: i64) -> Result<(), tokio_postgres::Error> {
    // (orderID, price, remaining, escrow) for updates; new ones apart.
    let mut updates: (Vec<i32>, Vec<f64>, Vec<i32>, Vec<f64>) = Default::default();
    let mut new: Vec<((i32, i32, bool), f64, i64, f64)> = Vec::new();
    let mut spent: HashMap<u32, f64> = HashMap::new();
    // What the book becomes once the database has it: (key, (left, escrow)
    // or gone).
    let mut after: Vec<((i32, i32, bool), Option<(i64, f64)>)> = Vec::new();
    let hubs = e.hubs.clone();
    let mut types: Vec<i32> = e.goods.keys().copied().collect();
    types.sort_unstable();
    for hub in hubs {
        let Some(&trader) = e.traders.get(&hub) else { continue };
        let wallet = bots[trader as usize].isk;
        let mut budget = wallet * ESCROW_SHARE;
        for &t in &types {
            let kind = e.goods[&t].kind;
            // Selling: everything in stock.
            let have = e.stock_at(hub, t);
            let ask = e.ask(hub, t);
            match (e.orders.get(&(hub, t, false)), have > 0) {
                (Some(o), _) => {
                    updates.0.push(o.id);
                    updates.1.push(ask);
                    updates.2.push(have.min(i32::MAX as i64) as i32);
                    updates.3.push(0.0);
                }
                (None, true) => new.push(((hub, t, false), ask, have, 0.0)),
                (None, false) => {}
            }
            if e.orders.contains_key(&(hub, t, false)) {
                after.push(((hub, t, false), (have > 0).then_some((have, 0.0))));
            }
            // Buying: ore and minerals short of enough.
            if kind == Kind::Hull {
                continue;
            }
            let bid = e.bid(hub, t);
            let enough = (ENOUGH_VALUE / e.goods[&t].bid.max(0.01)) as i64;
            let old = e.orders.get(&(hub, t, true)).cloned();
            let refund = old.as_ref().map_or(0.0, |o| o.escrow);
            let mut want = (enough - have).max(0);
            let afford = ((budget + refund) / bid.max(0.01)) as i64;
            want = want.min(afford).min(i32::MAX as i64 / 2);
            let escrow = want as f64 * bid;
            budget = (budget + refund - escrow).max(0.0);
            *spent.entry(trader).or_default() += escrow - refund;
            match old {
                Some(o) => {
                    updates.0.push(o.id);
                    updates.1.push(bid);
                    updates.2.push(want as i32);
                    updates.3.push(escrow);
                    after.push(((hub, t, true), (want > 0).then_some((want, escrow))));
                }
                None if want > 0 => new.push(((hub, t, true), bid, want, escrow)),
                None => {}
            }
        }
    }
    if !updates.0.is_empty() {
        db.execute(
            r#"UPDATE market_orders o SET "price" = u.price, "volRemaining" = u.remaining,
                      "volEntered" = GREATEST(o."volEntered", u.remaining), "escrow" = u.escrow, "issued" = $5::bigint,
                      "orderState" = CASE WHEN u.remaining > 0 THEN $6::int ELSE $7::int END
                 FROM unnest($1::int[], $2::float8[], $3::int[], $4::float8[]) AS u(id, price, remaining, escrow)
                WHERE o."orderID" = u.id"#,
            &[&updates.0, &updates.1, &updates.2, &updates.3, &now, &ORDER_OPEN, &ORDER_DONE],
        )
        .await?;
    }
    for (key, left) in after {
        match left {
            Some((listed, escrow)) => {
                if let Some(o) = e.orders.get_mut(&key) {
                    o.listed = listed;
                    o.escrow = escrow;
                }
            }
            None => {
                e.orders.remove(&key);
            }
        }
    }
    if !new.is_empty() {
        let col = |f: &dyn Fn(&((i32, i32, bool), f64, i64, f64)) -> i32| new.iter().map(f).collect::<Vec<i32>>();
        let type_ids = col(&|n| n.0 .1);
        let stations = col(&|n| n.0 .0);
        let chars = col(&|n| e.traders.get(&n.0 .0).map_or(0, |i| bots[*i as usize].id));
        let systems = col(&|n| e.station_system.get(&n.0 .0).copied().unwrap_or(0));
        let regions = col(&|n| e.station_system.get(&n.0 .0).and_then(|s| e.region_of_system.get(s)).copied().unwrap_or(0));
        let bids: Vec<i16> = new.iter().map(|n| i16::from(n.0 .2)).collect();
        let prices: Vec<f64> = new.iter().map(|n| n.1).collect();
        let volumes = col(&|n| n.2.min(i32::MAX as i64) as i32);
        let escrows: Vec<f64> = new.iter().map(|n| n.3).collect();
        let ids: Vec<i32> = db
            .query(
                r#"INSERT INTO market_orders
                       ("typeID","charID","regionID","stationID","range","bid","price","volEntered","volRemaining","issued",
                        "orderState","minVolume","contraband","accountID","duration","isCorp","solarSystemID","escrow","jumps")
                   SELECT u.type_id, u.char_id, u.region, u.station, $10::int, u.bid, u.price, u.volume, u.volume, $11::bigint,
                          $12::int, 1, 0, 1000, $13::int, 0, u.system, u.escrow, 1
                     FROM unnest($1::int[], $2::int[], $3::int[], $4::int[], $5::smallint[], $6::float8[], $7::int[], $8::int[], $9::float8[])
                          WITH ORDINALITY AS u(type_id, char_id, region, station, bid, price, volume, system, escrow, n)
                    ORDER BY u.n
                   RETURNING "orderID""#,
                &[&type_ids, &chars, &regions, &stations, &bids, &prices, &volumes, &systems, &escrows, &RANGE_STATION, &now, &ORDER_OPEN, &ORDER_DAYS],
            )
            .await?
            .iter()
            .map(|r| r.get::<_, i32>(0))
            .collect();
        for (n, id) in new.iter().zip(ids) {
            e.orders.insert(n.0, Order { id, listed: n.2, escrow: n.3 });
        }
    }
    for (trader, delta) in spent {
        if delta.abs() >= 0.01 {
            e.payments.push((trader, -delta));
        }
    }
    e.stats.orders = e.orders.len();
    Ok(())
}

/// The bots' own orders at start: those their traders hold, open.
pub async fn adopt_orders(e: &mut Economy, db: &Client, bots: &[Bot]) -> Result<(), tokio_postgres::Error> {
    let by_char: HashMap<i32, i32> = e.traders.iter().map(|(hub, i)| (bots[*i as usize].id, *hub)).collect();
    let chars: Vec<i32> = by_char.keys().copied().collect();
    let rows = db
        .query(
            r#"SELECT "orderID", "charID", "stationID", "typeID", "bid", "volRemaining", "escrow" FROM market_orders
                WHERE "charID" = ANY($1) AND "orderState" = $2"#,
            &[&chars, &ORDER_OPEN],
        )
        .await?;
    let mut stale = Vec::new();
    for r in rows {
        let id = int(&r, 0).unwrap_or(0) as i32;
        let (char_id, station, t, bid) = (int(&r, 1).unwrap_or(0) as i32, int(&r, 2).unwrap_or(0) as i32, int(&r, 3).unwrap_or(0) as i32, int(&r, 4) == Some(1));
        if by_char.get(&char_id) != Some(&station) || e.orders.contains_key(&(station, t, bid)) {
            stale.push(id);
            continue;
        }
        e.orders.insert((station, t, bid), Order { id, listed: int(&r, 5).unwrap_or(0), escrow: float(&r, 6).unwrap_or(0.0) });
    }
    if !stale.is_empty() {
        // Orders of traders who are no longer a hub's: closed, escrow returned.
        db.execute(
            r#"WITH closed AS (UPDATE market_orders SET "orderState" = $2, "volRemaining" = 0 WHERE "orderID" = ANY($1)
                               RETURNING "charID", "escrow")
               UPDATE "character_" c SET "balance" = c."balance" + s.escrow
                 FROM (SELECT "charID", sum("escrow") AS escrow FROM closed GROUP BY 1) s WHERE c."characterID" = s."charID""#,
            &[&stale, &ORDER_DONE],
        )
        .await?;
    }
    e.stats.orders = e.orders.len();
    Ok(())
}

/// Players' buy orders the bots' stock reaches at their price: filled, as a
/// seller would, out of the hub's stock.
async fn fill_player_bids(e: &mut Economy, db: &Client, db_url: &str, world: &crate::world::World, bots: &[Bot]) -> Result<(), tokio_postgres::Error> {
    let goods: Vec<i32> = e.goods.keys().copied().collect();
    let sql = r#"SELECT o."orderID", o."charID", o."stationID", o."typeID", o."price", o."volRemaining", o."escrow", o."regionID",
                        o."solarSystemID", o."range"
                   FROM market_orders o
                  WHERE o."bid" = 1 AND o."orderState" = 0 AND o."volRemaining" > 0 AND o."charID" >= $2
                    AND o."typeID" = ANY($1) AND o."isCorp" = 0
                    AND NOT EXISTS (SELECT 1 FROM "bots" b WHERE b."characterID" = o."charID")"#;
    let rows = db.query(sql, &[&goods, &FIRST_PLAYER_ID]).await?;
    if rows.is_empty() {
        return Ok(());
    }
    // (order, buyer, station, type, price, units, remaining, escrow, seller)
    let mut fills: Vec<(i32, i32, i32, i32, f64, i64, i64, f64, i32)> = Vec::new();
    for r in &rows {
        let (id, buyer, station, t) = (int(r, 0).unwrap_or(0) as i32, int(r, 1).unwrap_or(0) as i32, int(r, 2).unwrap_or(0) as i32, int(r, 3).unwrap_or(0) as i32);
        let (price, remaining, escrow) = (float(r, 4).unwrap_or(0.0), int(r, 5).unwrap_or(0), float(r, 6).unwrap_or(0.0));
        let (region, system, range) = (int(r, 7).unwrap_or(0) as i32, int(r, 8).unwrap_or(0) as i32, int(r, 9).unwrap_or(-1) as i32);
        // A hub in the order's reach whose stock and price will do.
        let hub = e.hubs.iter().copied().find(|h| {
            let hs = e.station_system.get(h).copied().unwrap_or(0);
            let reach = (range == RANGE_STATION && *h == station) || (range == 0 && hs == system) || (range > 0 && e.region_of_system.get(&hs) == Some(&region));
            reach && e.stock_at(*h, t) > 0 && e.ask(*h, t) <= price && e.traders.contains_key(h)
        });
        let Some(hub) = hub else { continue };
        let n = remaining.min(e.stock_at(hub, t));
        e.add(hub, t, -n);
        let seller = bots[e.traders[&hub] as usize].id;
        fills.push((id, buyer, station, t, price, n, remaining, escrow, seller));
    }
    for (id, buyer, station, t, price, n, remaining, escrow, seller) in fills {
        let amount = price * n as f64;
        let left = remaining - n;
        let mut client = crate::db::connect(db_url).await?;
        let mut txn = crate::items::ItemTxn::begin(&mut client).await?;
        let stack = crate::items::NewItem { type_id: t as i64, owner_id: buyer as i64, location_id: station as i64, flag: FLAG_HANGAR as i64, quantity: n, singleton: false };
        txn.stack_onto(stack).await?;
        txn.tx()
            .execute(
                r#"UPDATE market_orders SET "volRemaining" = $2, "orderState" = $3, "escrow" = $4 WHERE "orderID" = $1"#,
                &[&id, &(left as i32), &(if left == 0 { ORDER_DONE } else { ORDER_OPEN }), &(escrow - amount).max(0.0)],
            )
            .await?;
        let entry = crate::svc::wallet::Entry { ref_type: REF_MARKET_TRANSACTION, payer: buyer as i64, payee: seller as i64, arg_id1: "", reason: "" };
        let paid = crate::svc::wallet::post(txn.tx(), seller as i64, amount, &entry).await;
        if let Err(crate::svc::wallet::Transfer::Db(err)) = paid {
            return Err(err);
        }
        let region = e.station_system.get(&station).and_then(|s| e.region_of_system.get(s)).copied().unwrap_or(0);
        let now = crate::bulkdata::filetime_now();
        txn.tx()
            .execute(
                r#"INSERT INTO market_transactions ("transactionDateTime","typeID","quantity","price","transactionType","clientID",
                                                    "regionID","stationID","characterID","isCorp","accountKey")
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,0,1000), ($1,$2,$3,$4,$10,$9,$7,$8,$6,0,1000)"#,
                &[&now, &t, &(n as i32), &price, &TRANSACTION_PURCHASE, &seller, &region, &station, &buyer, &TRANSACTION_SALE],
            )
            .await?;
        let events = txn.commit().await?;
        e.stats.player_orders_filled += 1;
        e.stats.sold_to_players += amount;
        let w = world.lock();
        let clients = w.clients_of(buyer as i64);
        for event in events {
            w.notify(&clients, "OnItemChange", event);
        }
        info!("bots: filled {n} of type {t} on {buyer}'s buy order {id} at {price}");
    }
    Ok(())
}

/// The bots' trade among themselves, one line a hub and good, into the
/// market's transactions (the price history players see).
async fn history(e: &mut Economy, db: &Client, bots: &[Bot], now: i64) -> Result<(), tokio_postgres::Error> {
    if e.trades.is_empty() {
        return Ok(());
    }
    let trades: Vec<((i32, i32), (i64, f64))> = e.trades.drain().collect();
    let mut cols: (Vec<i32>, Vec<i32>, Vec<f64>, Vec<i32>, Vec<i32>, Vec<i32>) = Default::default();
    for ((station, t), (n, value)) in trades {
        let Some(&trader) = e.traders.get(&station) else { continue };
        cols.0.push(t);
        cols.1.push(n.min(i32::MAX as i64) as i32);
        cols.2.push(value / n.max(1) as f64);
        cols.3.push(e.station_system.get(&station).and_then(|s| e.region_of_system.get(s)).copied().unwrap_or(0));
        cols.4.push(station);
        cols.5.push(bots[trader as usize].id);
    }
    db.execute(
        r#"INSERT INTO market_transactions ("transactionDateTime","typeID","quantity","price","transactionType","clientID",
                                            "regionID","stationID","characterID","isCorp","accountKey")
           SELECT $7::bigint, u.t, u.n, u.price, $8::int, 0, u.region, u.station, u.who, 0, 1000
             FROM unnest($1::int[], $2::int[], $3::float8[], $4::int[], $5::int[], $6::int[]) AS u(t, n, price, region, station, who)"#,
        &[&cols.0, &cols.1, &cols.2, &cols.3, &cols.4, &cols.5, &now, &TRANSACTION_SALE],
    )
    .await?;
    Ok(())
}

/// Player corporations' tax rates, for the bots that have joined them.
pub async fn read_tax_rates(e: &mut Economy, db: &Client, corps: &[i32]) -> Result<(), tokio_postgres::Error> {
    let rows = db.query(r#"SELECT "corporationID", COALESCE("taxRate", 0)::float8 FROM "corporation" WHERE "corporationID" = ANY($1)"#, &[&corps]).await?;
    e.tax_rates = rows.iter().map(|r| (int(r, 0).unwrap_or(0) as i32, float(r, 1).unwrap_or(0.0))).collect();
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Veldspar refining to Tritanium; a hull built of Tritanium; a hub at
    /// station 301 (system 3), a mining station 101 (system 1).
    pub fn economy() -> Economy {
        let mut e = Economy::empty();
        let good = |name: &str, kind, volume, bid: f64| Good { name: name.into(), kind, volume, ask: bid * 2.0, bid };
        e.goods.insert(1230, good("Veldspar", Kind::Ore, 0.1, 1.5));
        e.goods.insert(34, good("Tritanium", Kind::Mineral, 0.01, 1.0));
        e.goods.insert(587, good("Rifter", Kind::Hull, 2_500.0, 150_000.0));
        e.refine.insert(1230, (333, vec![(34, 1000)]));
        e.bom.insert(587, vec![(34, 20_000)]);
        e.hubs = vec![301];
        e.station_system = HashMap::from([(101, 1), (301, 3)]);
        e.hub_by_system = HashMap::from([(1, 301), (2, 301), (3, 301)]);
        e.region_of_system = HashMap::from([(1, 1), (2, 1), (3, 1)]);
        e.traders.insert(301, 0);
        e
    }

    #[test]
    fn ore_sold_at_home_is_refined_hauled_and_built_into_hulls() {
        let mut e = economy();
        let mut hold = vec![(1230, 333 * 30)];
        let earned = e.sell_ore(101, &mut hold);
        assert!(earned > 0.0 && hold.is_empty());
        e.refine_all();
        assert_eq!(e.stock_at(101, 34), 24_000);
        assert_eq!(e.stock_at(101, 1230), 0);
        // A hauler takes the minerals to the hub.
        let mut load = e.load(101, 1_000.0);
        assert_eq!(load, vec![(34, 24_000)]);
        let pay = e.unload(301, &mut load);
        assert!(pay > 0.0);
        e.build(&mut || 0.0);
        assert_eq!(e.stock_at(301, 587), 1);
        assert_eq!(e.stock_at(301, 34), 4_000);
    }

    #[test]
    fn prices_fall_as_stock_grows_and_stay_inside_the_npc_spread() {
        let mut e = economy();
        let none = e.ask(301, 34);
        e.add(301, 34, 100_000_000);
        let glut = e.ask(301, 34);
        let g = &e.goods[&34];
        assert!(none > glut, "{none} {glut}");
        assert!(none < g.ask && glut > g.bid);
        assert!(e.bid(301, 34) < glut && e.bid(301, 34) > g.bid);
    }

    #[test]
    fn a_hull_bought_from_stock_pays_the_hubs_trader_and_a_missing_one_is_remembered() {
        let mut e = economy();
        e.add(301, 587, 2);
        let paid = e.buy_hull(587, 101, 1e9, 0.0).unwrap();
        assert_eq!(e.stock_at(301, 587), 1);
        assert_eq!(e.payments, vec![(0, paid)]);
        e.add(301, 587, -1);
        let npc = e.buy_hull(587, 101, 1e9, 0.0).unwrap();
        assert_eq!(npc, 300_000.0);
        assert!(e.shortages().iter().any(|(t, _)| *t == 587));
        assert_eq!(e.buy_hull(587, 101, 10.0, 0.0), None);
    }

    #[test]
    fn tax_goes_to_the_corporation_and_a_share_to_its_alliance() {
        let mut e = economy();
        e.executor_of.insert(5, 9);
        let tax = e.tax(5, 1_000_000.0, true);
        assert_eq!(tax, 100_000.0);
        assert!((e.corp_income[&5] - 80_000.0).abs() < 0.01 && (e.corp_income[&9] - 20_000.0).abs() < 0.01);
        e.tax_rates.insert(7, 0.25);
        assert_eq!(e.tax(7, 1_000.0, false), 250.0);
    }
}
