//! The bot population: characters the server plays, so the universe is not
//! empty. `RMR_BOTS` sets how many there are (made on first start, kept
//! after); `RMR_BOTS_SPEED` runs their clock faster, for testing; `/bots
//! on|off|status` pauses and resumes them while the server runs, and `/bots
//! event|war|density|follow` steers them (as the dashboard can).
//!
//! Each bot is stepped (`behaviour::step`) when its next moment comes up,
//! out of one queue ordered by time. Their state lives here; every few
//! seconds what changed is written back in one batch, and the star map's
//! pilot counts take in where the online ones are. Where a player is in
//! space, the bots there are ships (`presence`). Most belong to bot
//! corporations and alliances, which fight wars among themselves
//! (`politics`), hold null-sec with control towers (`sov`) and now and then
//! betray, merge, collapse, gang up and raid (`events`).
//!
//! They make and spend money in an economy of their own that reaches the
//! real market (`economy`), climb from frigates to battleships as they get
//! richer (`ships`), remember the players who kill them and reward those
//! who kill their enemies (`relations`), can be hired as wingmen (`hire`),
//! and apply to player corporations.

pub mod behaviour;
mod battle;
mod control;
mod defense;
mod economy;
mod events;
mod hire;
mod panels;
mod politics;
mod population;
mod presence;
mod relations;
mod schema;
mod ships;
mod sov;

pub use presence::eject;
pub use relations::{stance, Stance};

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio_postgres::Client;
use tracing::{error, info, warn};

use crate::combat::Rng;
use crate::conn::Server;
use behaviour::{Atlas, Bot, Ctx};
use presence::{Identity, Presence};

/// How often the clock moves and due bots are stepped.
const TICK: Duration = Duration::from_millis(250);
/// How often what changed is saved and the map's counts refreshed.
const FLUSH: Duration = Duration::from_secs(10);
/// How often the bots' wallets are read back, for what players paid them.
const BALANCES: Duration = Duration::from_secs(120);
/// How often, on the bots' clock, politics moves and enemies in the same
/// place may fight.
const POLITICS_EVERY: u32 = 600;
const SKIRMISH_EVERY: u32 = 60;
/// How often space is brought up to date with the bots.
const SYNC: Duration = Duration::from_secs(1);
/// How often the run is summed up in the log.
const REPORT: Duration = Duration::from_secs(60);
/// Corporations at or above this ID are players' or bots', not NPCs'.
const FIRST_OWNED_CORP: i32 = 2_000_000;

static RUNNING: AtomicBool = AtomicBool::new(true);
static CONTROLS: AtomicBool = AtomicBool::new(false);
static STATUS: Mutex<String> = Mutex::new(String::new());
static DB_URL: OnceLock<String> = OnceLock::new();

/// /bots on|off.
pub fn set_running(on: bool) {
    RUNNING.store(on, Ordering::Relaxed);
}

/// /bots status: the last summary.
pub fn status() -> String {
    let s = STATUS.lock().map(|s| s.clone()).unwrap_or_default();
    if s.is_empty() { "The bots are not running.".into() } else { s }
}

/// The settings that are not the simulation's size: whether pirates attack
/// players on sight, and whether the dashboard may steer the bots.
pub fn configure(pirates_attack_players: bool, dashboard_controls: bool) {
    presence::set_pirates_attack_players(pirates_attack_players);
    CONTROLS.store(dashboard_controls, Ordering::Relaxed);
}

pub fn controls_enabled() -> bool {
    CONTROLS.load(Ordering::Relaxed)
}

/// A request to steer the bots, from `/bots` or the dashboard: queued for
/// the simulation's next look. Returns what was understood.
pub fn request(words: &[&str]) -> Result<String, String> {
    let r = control::parse(words)?;
    let said = match &r {
        control::Request::Event(k) => format!("Asked for a {k:?}."),
        control::Request::War(a, b) => format!("Asked for war between {a} and {b}."),
        control::Request::Density(r, f) => format!("Asked for {r} at {f}x."),
        control::Request::Follow(n) => format!("Following {n}."),
        control::Request::Unfollow(n) => format!("No longer following {n}."),
    };
    control::ask(r);
    Ok(said)
}

/// The database the bot simulation writes to.
pub fn db_url() -> String {
    DB_URL.get().cloned().unwrap_or_default()
}

pub async fn run(server: Arc<Server>, target: usize, speed: f64, shown: usize) {
    if target == 0 {
        return;
    }
    let _ = DB_URL.set(server.db_url.clone());
    match start(&server, target).await {
        Ok((mut db, atlas, mut bots, mut ids)) => {
            let mut rng = Rng::seeded(crate::bulkdata::filetime_now() as u64 ^ 0x5eed);
            let politics = match politics::load(&db, &atlas, 0, speed).await {
                Ok(p) if !p.corps.is_empty() => Ok(p),
                Ok(_) => politics::form(&mut db, &atlas, &mut bots, &mut ids, &mut rng, 0, speed).await,
                Err(e) => Err(e),
            };
            let mut politics = match politics {
                Ok(p) => p,
                Err(e) => return error!("bots: politics: {}", crate::logging::chain(&e)),
            };
            match sov::establish(&db, &atlas, &politics, &mut rng, server.sov_claim_delay_secs).await {
                Ok(news) if !news.is_empty() => {
                    politics.tell(news);
                    crate::svc::starbase::sovereignty(&db, server.sov_claim_delay_secs).await;
                }
                Ok(_) => {}
                Err(e) => error!("bots: settling null-sec: {}", crate::logging::chain(&e)),
            }
            let (yard, econ) = match markets(&db, &atlas, &mut bots, &ids, &politics).await {
                Ok(v) => v,
                Err(e) => return error!("bots: the economy: {}", crate::logging::chain(&e)),
            };
            let mut presence = Presence::new(presence::hulls(&db, &ids, yard.all().map(i64::from)).await, shown);
            let yard = Arc::new(yard);
            presence.yard = Some(Arc::clone(&yard));
            let relations = match relations::load(&db).await {
                Ok(r) => r,
                Err(e) => return error!("bots: relations: {}", crate::logging::chain(&e)),
            };
            let density = read_density(&db).await;
            let names = panels::names(&db).await;
            let types = character_types(&db).await;
            let sim = Sim::new(server, db, atlas, bots, ids, types, presence, politics, econ, yard, relations, density, names, speed);
            sim.run().await
        }
        Err(e) => error!("bots: {e}"),
    }
}

async fn start(server: &Server, target: usize) -> Result<(Client, Atlas, Vec<Bot>, Vec<Identity>), String> {
    let mut db = crate::db::connect(&server.db_url).await.map_err(|e| crate::logging::chain(&e).to_string())?;
    schema::ensure(&db).await.map_err(|e| format!("the bots' tables: {}", crate::logging::chain(&e)))?;
    let started = Instant::now();
    let atlas = population::atlas(&db).await?;
    info!("bots: map of {} systems, {} stations read in {:.1}s", atlas.systems.len(), atlas.station_system.len(), started.elapsed().as_secs_f64());
    let have = population::ensure(&mut db, &atlas, target).await?;
    let (bots, ids) = population::load(&db, &atlas, target).await.map_err(|e| crate::logging::chain(&e).to_string())?;
    info!("bots: {} loaded of {have}, in {:.1}s", bots.len(), started.elapsed().as_secs_f64());
    Ok((db, atlas, bots, ids))
}

/// The ships bots fly and the economy they trade in: hulls and their
/// prices, each bot's place on its ladder, and the market hubs (the trade
/// hubs and each null-sec alliance's capital, where most of its pilots live).
async fn markets(db: &Client, atlas: &Atlas, bots: &mut [Bot], ids: &[Identity], p: &politics::Politics) -> Result<(ships::Shipyard, economy::Economy), tokio_postgres::Error> {
    let hulls = ships::hull_types(db).await?;
    let mut econ = economy::load(db, atlas, &hulls).await?;
    let prices: HashMap<i32, f64> = econ.goods.iter().filter(|(_, g)| g.kind == economy::Kind::Hull).map(|(t, g)| (*t, g.ask)).collect();
    let yard = ships::Shipyard::load(db, &prices).await?;
    for (b, id) in bots.iter_mut().zip(ids) {
        b.hull = id.hull as i32;
        b.tier = yard.tier_of(b.role, b.hull);
        b.prime = id.alliance.map_or_else(|| behaviour::prime_for(b.id), |a| p.prime_of(a));
    }
    let mut homes: HashMap<i32, HashMap<i32, u32>> = HashMap::new();
    for (b, id) in bots.iter().zip(ids) {
        if let Some(a) = id.alliance.filter(|a| p.alliances.get(a).is_some_and(|x| x.kind == politics::KIND_NULL_ALLIANCE)) {
            *homes.entry(a).or_default().entry(b.home).or_default() += 1;
        }
    }
    let capitals = homes.values().filter_map(|h| h.iter().max_by_key(|(s, n)| (**n, Reverse(**s))).map(|(s, _)| *s));
    let hubs: Vec<i32> = atlas.hubs.iter().copied().chain(capitals).collect();
    economy::set_hubs(&mut econ, atlas, hubs, bots);
    economy::adopt_orders(&mut econ, db, bots).await?;
    info!("bots: {} hulls on the ladders, {} market hubs, {} traders, {} standing orders", yard.hulls.len(), econ.hubs.len(), econ.traders.len(), econ.stats.orders);
    Ok((yard, econ))
}

async fn read_density(db: &Client) -> HashMap<i32, f32> {
    match db.query(r#"SELECT "regionID", "factor" FROM "botRegionDensity""#, &[]).await {
        Ok(rows) => rows.iter().map(|r| (r.get::<_, i32>(0), r.get::<_, f32>(1))).collect(),
        Err(e) => {
            error!("bots: reading region density: {}", crate::logging::chain(&e));
            HashMap::new()
        }
    }
}

/// What a run has done since the last report.
#[derive(Default)]
struct Tally {
    steps: u64,
    step_time: Duration,
    worst_tick: Duration,
    flushes: u32,
    flushed_rows: u64,
    flush_time: Duration,
    worst_flush: Duration,
}

/// Everything the simulation holds while it runs.
struct Sim {
    server: Arc<Server>,
    db: Client,
    atlas: Atlas,
    bots: Vec<Bot>,
    ids: Vec<Identity>,
    types: HashMap<i32, i64>,
    presence: Presence,
    politics: politics::Politics,
    econ: economy::Economy,
    yard: Arc<ships::Shipyard>,
    relations: relations::Relations,
    events: events::Events,
    hires: hire::Hires,
    following: control::Following,
    density: HashMap<i32, f32>,
    names: panels::Names,
    speed: f64,
    by_char: HashMap<i64, u32>,
    sieges: Vec<sov::Siege>,
    defenses: Vec<defense::Defense>,
    changes: Vec<presence::Change>,
    rng: Rng,
    clock: f64,
    day0: u32,
    due: BinaryHeap<Reverse<(u32, u32, u32)>>,
    generation: Vec<u32>,
    dirty: Vec<bool>,
    changed: Vec<u32>,
    hulls_changed: Vec<u32>,
    said: std::collections::VecDeque<(i64, String)>,
    tally: Tally,
}

impl Sim {
    #[allow(clippy::too_many_arguments)]
    fn new(
        server: Arc<Server>,
        db: Client,
        atlas: Atlas,
        bots: Vec<Bot>,
        ids: Vec<Identity>,
        types: HashMap<i32, i64>,
        presence: Presence,
        politics: politics::Politics,
        econ: economy::Economy,
        yard: Arc<ships::Shipyard>,
        relations: relations::Relations,
        density: HashMap<i32, f32>,
        names: panels::Names,
        speed: f64,
    ) -> Sim {
        let n = bots.len();
        let by_char: HashMap<i64, u32> = bots.iter().enumerate().map(|(i, b)| (b.id as i64, i as u32)).collect();
        let unix = (crate::bulkdata::filetime_now() / 10_000_000 - 11_644_473_600).max(0);
        Sim {
            server,
            db,
            atlas,
            bots,
            ids,
            types,
            presence,
            politics,
            econ,
            yard,
            relations,
            events: events::Events::default(),
            hires: hire::Hires::default(),
            following: control::Following::default(),
            density,
            names,
            speed,
            by_char,
            sieges: Vec::new(),
            defenses: Vec::new(),
            changes: Vec::new(),
            rng: Rng::seeded(crate::bulkdata::filetime_now() as u64),
            clock: 0.0,
            day0: (unix % 86_400) as u32,
            due: BinaryHeap::with_capacity(n),
            generation: vec![0; n],
            dirty: vec![true; n],
            changed: (0..n as u32).collect(),
            hulls_changed: Vec::new(),
            said: std::collections::VecDeque::new(),
            tally: Tally::default(),
        }
    }

    fn touch(&mut self, i: u32) {
        if !self.dirty[i as usize] {
            self.dirty[i as usize] = true;
            self.changed.push(i);
        }
    }

    /// Moves a bot's next step to `wait` seconds from now.
    fn reschedule(&mut self, i: u32, wait: u32) {
        self.generation[i as usize] += 1;
        self.due.push(Reverse((self.clock as u32 + wait, i, self.generation[i as usize])));
        self.touch(i);
    }

    fn say(&mut self, text: String) {
        info!("bots: {text}");
        self.said.push_back((crate::bulkdata::filetime_now(), text));
        while self.said.len() > 20 {
            self.said.pop_front();
        }
    }

    async fn run(mut self) {
        {
            let ctx = Ctx { atlas: &self.atlas, econ: &mut self.econ, yard: Some(&*self.yard), day0: self.day0, density: &self.density };
            for (i, bot) in self.bots.iter_mut().enumerate() {
                let wait = behaviour::wake_up(bot, 0, &ctx, &mut self.rng);
                self.due.push(Reverse((wait, i as u32, 0)));
            }
            ctx.econ.payments.clear();
        }
        {
            let mut w = self.server.world.lock();
            w.bot_roster = roster(&self.bots, &self.ids, &self.types);
            w.bot_index = self.by_char.clone();
        }
        self.refresh_politics_figures().await;
        let (mut next_politics, mut next_skirmish) = (POLITICS_EVERY, SKIRMISH_EVERY);
        let mut last_sync = Instant::now();
        let (mut last_flush, mut last_report, mut last_tick, mut last_balances) = (Instant::now(), Instant::now(), Instant::now(), Instant::now());
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        info!("bots: running {} at {}x", self.bots.len(), self.speed);
        loop {
            interval.tick().await;
            let elapsed = last_tick.elapsed();
            last_tick = Instant::now();
            let running = RUNNING.load(Ordering::Relaxed);
            if running {
                self.clock += elapsed.as_secs_f64() * self.speed;
                let now = self.clock as u32;
                self.politics.hour = ((self.day0 as u64 + now as u64) % 86_400) as f64 / 3_600.0;
                let work = Instant::now();
                self.step_due(now);
                if now >= next_skirmish {
                    next_skirmish = now + SKIRMISH_EVERY;
                    self.skirmish(now);
                }
                let took = work.elapsed();
                self.tally.step_time += took;
                self.tally.worst_tick = self.tally.worst_tick.max(took);
                if now >= next_politics {
                    next_politics = now + POLITICS_EVERY;
                    self.politics_tick(now).await;
                }
            }
            if last_sync.elapsed() >= SYNC {
                last_sync = Instant::now();
                self.sync(running).await;
            }
            if last_flush.elapsed() >= FLUSH {
                last_flush = Instant::now();
                self.flush_all(running).await;
                let took = last_flush.elapsed();
                self.tally.flushes += 1;
                self.tally.flush_time += took;
                self.tally.worst_flush = self.tally.worst_flush.max(took);
                if last_balances.elapsed() >= BALANCES {
                    last_balances = Instant::now();
                    if let Err(e) = reload_balances(&self.db, &mut self.bots, &self.by_char).await {
                        error!("bots: reading wallets back: {}", crate::logging::chain(&e));
                    }
                }
            }
            if last_report.elapsed() >= REPORT {
                let mut summary = report(&self.bots, &self.tally, last_report.elapsed(), self.clock, self.speed, running);
                let s = &self.econ.stats;
                summary.push_str(&format!(
                    "; {} bot ship(s) shown in {} watched system(s); {}; economy: {} hulls built, {} bought ({} from NPCs), {:.0}M ISK sold to players, {} orders; {} wingmen, {} hunting",
                    self.presence.shown(),
                    self.presence.observed(),
                    self.politics.summary(),
                    s.hulls_built,
                    s.hulls_bought,
                    s.hulls_bought_from_npcs,
                    s.sold_to_players / 1.0e6,
                    s.orders,
                    self.hires.by_bot.len(),
                    self.relations.hunting.len(),
                ));
                self.politics.kills = 0;
                info!("bots: {summary}");
                if let Ok(mut st) = STATUS.lock() {
                    *st = summary;
                }
                if self.tally.worst_flush > FLUSH / 2 {
                    warn!("bots: a save took {:.1}s, half the time between saves", self.tally.worst_flush.as_secs_f64());
                }
                self.tally = Tally::default();
                last_report = Instant::now();
            }
        }
    }

    /// Steps every bot whose moment has come, and settles what the economy
    /// paid out meanwhile.
    fn step_due(&mut self, now: u32) {
        let mut ctx = Ctx { atlas: &self.atlas, econ: &mut self.econ, yard: Some(&*self.yard), day0: self.day0, density: &self.density };
        while let Some(&Reverse((at, i, gen))) = self.due.peek() {
            if at > now {
                break;
            }
            self.due.pop();
            if gen != self.generation[i as usize] {
                continue;
            }
            let bot = &mut self.bots[i as usize];
            let before = (bot.system, bot.station, bot.activity);
            let wait = behaviour::step(bot, now, &mut ctx, &mut self.rng).max(1);
            if bot.earned > 0.0 {
                let corp = self.ids[i as usize].corp;
                if corp >= FIRST_OWNED_CORP {
                    let tax = ctx.econ.tax(corp, bot.earned, self.politics.corps.contains_key(&corp));
                    bot.isk -= tax;
                }
                bot.earned = 0.0;
            }
            if bot.hull as i64 != self.ids[i as usize].hull && bot.hull != 0 {
                self.ids[i as usize].hull = bot.hull as i64;
                self.hulls_changed.push(i);
            }
            self.presence.note(i, before, bot, &mut self.changes);
            self.due.push(Reverse((now + wait, i, gen)));
            self.tally.steps += 1;
            if !self.dirty[i as usize] {
                self.dirty[i as usize] = true;
                self.changed.push(i);
            }
        }
        let payments = std::mem::take(&mut ctx.econ.payments);
        for (i, amount) in payments {
            self.bots[i as usize].isk += amount;
            self.touch(i);
        }
    }

    /// Every minute of the bots' clock: fights nobody watches, raids, and
    /// hunts for players.
    fn skirmish(&mut self, now: u32) {
        let watched = self.presence.watched().clone();
        let lost = politics::skirmish(&mut self.politics, &self.atlas, &mut self.bots, &self.ids, &watched, &mut self.rng);
        let (raided, news) = events::raids(&mut self.events, &self.atlas, &mut self.politics, &mut self.bots, &mut self.presence, &mut self.rng, now);
        self.politics.tell(news);
        let players: Vec<(i32, i32)> = self.server.world.lock().players().iter().filter(|p| !p.docked).map(|p| (p.char_id as i32, p.solar_system_id as i32)).collect();
        let presence = &self.presence;
        let p = &self.politics;
        let hunted = self.relations.hunt(
            &self.atlas,
            &mut self.bots,
            &self.ids,
            |id| p.side_of(id.corp),
            |id| presence.armed(id.hull),
            &players,
            now,
            &mut self.rng,
        );
        self.presence.set_hunts(self.relations.hunting.iter().map(|(i, (pl, _))| (*i, *pl as i64)).collect());
        for (i, wait) in lost.into_iter().chain(raided).chain(hunted) {
            self.reschedule(i, wait);
        }
    }

    /// Every ten minutes of the bots' clock: politics, sovereignty, the
    /// economy's slower half, corporations' applications, and events.
    async fn politics_tick(&mut self, now: u32) {
        if let Err(e) = politics::tick(&mut self.politics, &self.db, &self.bots, &mut self.ids, &mut self.rng, now, self.speed).await {
            error!("bots: politics: {}", crate::logging::chain(&e));
        }
        match sov::upkeep(&self.db, &self.atlas, &mut self.politics, self.presence.watched(), &mut self.sieges, &mut self.rng, now, self.server.sov_claim_delay_secs, self.speed).await {
            Ok(news) => self.politics.tell(news),
            Err(e) => error!("bots: sovereignty: {}", crate::logging::chain(&e)),
        }
        let mut roll = {
            let mut r = Rng::seeded(self.rng.below(u32::MAX as usize) as u64);
            move || r.unit()
        };
        if let Err(e) = economy::tick(&mut self.econ, &self.db, &self.server.db_url, &self.server.world, &self.bots, &mut roll).await {
            error!("bots: the economy: {}", crate::logging::chain(&e));
        }
        if let Err(e) = relations::answer_applications(&self.relations, &self.db, &self.server.world, &self.politics).await {
            error!("bots: applications to bot corporations: {}", crate::logging::chain(&e));
        }
        match relations::recruit(&self.db, &self.server.world, &self.atlas, &self.politics, &mut self.bots, &mut self.ids, &self.by_char, &mut self.rng).await {
            Ok(moved) => {
                for i in moved {
                    self.touch(i);
                }
            }
            Err(e) => error!("bots: applications to player corporations: {}", crate::logging::chain(&e)),
        }
        self.run_events(Vec::new(), true, now).await;
        self.relations.cool(now);
        self.refresh_politics_figures().await;
        // Corporations changed: Local shows them in their new ones.
        let fresh = roster(&self.bots, &self.ids, &self.types);
        self.server.world.lock().bot_roster = fresh;
    }

    /// Events, rolled for or asked for; returns how many happened.
    async fn run_events(&mut self, asked: Vec<events::Kind>, roll: bool, now: u32) -> usize {
        match events::tick(&mut self.events, &self.db, &self.atlas, &mut self.politics, &mut self.bots, &mut self.ids, &mut self.presence, asked, roll, &mut self.rng, now).await {
            Ok((news, moved)) => {
                let happened = news.len();
                self.politics.tell(news);
                for (i, wait) in moved {
                    self.reschedule(i, wait);
                }
                happened
            }
            Err(e) => {
                error!("bots: events: {}", crate::logging::chain(&e));
                0
            }
        }
    }

    /// Alliances' treasuries and prime times, the corporations' taxes, and
    /// each bot's prime time.
    async fn refresh_politics_figures(&mut self) {
        let alliances: Vec<i32> = self.politics.alliances.keys().copied().collect();
        let sql = r#"SELECT a."allianceID", co."balance" FROM alliances a JOIN "corporation" co ON co."corporationID" = a."executorCorpID" WHERE a."allianceID" = ANY($1)"#;
        match self.db.query(sql, &[&alliances]).await {
            Ok(rows) => self.politics.treasury = rows.iter().map(|r| (r.get::<_, i32>(0), r.get::<_, f64>(1))).collect(),
            Err(e) => error!("bots: reading the alliances' wallets: {}", crate::logging::chain(&e)),
        }
        let sql = r#"SELECT co."corporationID", a."executorCorpID" FROM "corporation" co JOIN alliances a ON a."allianceID" = co."allianceID"
                      JOIN "botGroups" g ON g."ownerID" = co."corporationID""#;
        match self.db.query(sql, &[]).await {
            Ok(rows) => self.econ.executor_of = rows.iter().map(|r| (r.get::<_, i32>(0), r.get::<_, i32>(1))).collect(),
            Err(e) => error!("bots: reading the alliances' executors: {}", crate::logging::chain(&e)),
        }
        let mut player_corps: Vec<i32> = self.ids.iter().map(|id| id.corp).filter(|c| *c >= FIRST_OWNED_CORP && !self.politics.corps.contains_key(c)).collect();
        player_corps.sort_unstable();
        player_corps.dedup();
        if let Err(e) = economy::read_tax_rates(&mut self.econ, &self.db, &player_corps).await {
            error!("bots: reading tax rates: {}", crate::logging::chain(&e));
        }
        for (b, id) in self.bots.iter_mut().zip(&self.ids) {
            b.prime = id.alliance.map_or_else(|| behaviour::prime_for(b.id), |a| self.politics.prime_of(a));
        }
    }

    /// Once a second: fleets and defences, requests from outside, hires,
    /// space brought up to date, what players did to bots, and the stances
    /// space reads.
    async fn sync(&mut self, running: bool) {
        let now = self.clock as u32;
        if running {
            let mut report = defense::run(&mut self.defenses, &self.server.world, &self.db, &self.atlas, &mut self.bots, &self.ids, &mut self.presence, &self.politics, &mut self.rng, now).await;
            if !self.sieges.is_empty() {
                let fought = battle::run(
                    &mut self.sieges,
                    &self.server.world,
                    &self.db,
                    &self.atlas,
                    &mut self.bots,
                    &self.ids,
                    &mut self.presence,
                    &mut self.politics,
                    &mut self.rng,
                    now,
                    self.server.sov_claim_delay_secs,
                    self.speed,
                )
                .await;
                match fought {
                    Ok(r) => report.merge(r),
                    Err(e) => error!("bots: sieges: {}", crate::logging::chain(&e)),
                }
            }
            self.politics.tell(report.news);
            for (side, player) in report.aggressors {
                self.relations.tower_attacked(side, player as i32, now);
            }
            for i in report.touched {
                self.touch(i);
            }
            // Sent home: they set off now.
            for i in report.released {
                self.reschedule(i, 30);
                self.changes.push(presence::Change::Busy { bot: i });
            }
            self.requests(now).await;
            let hired = hire::run(
                &mut self.hires,
                &self.server.world,
                &self.db,
                &self.server.db_url,
                &self.atlas,
                &mut self.bots,
                &self.ids,
                &mut self.presence,
                Some(&*self.yard),
                &self.by_char,
                &mut self.rng,
            )
            .await;
            for (i, wait) in hired {
                self.reschedule(i, wait);
                self.changes.push(presence::Change::Busy { bot: i });
            }
        }
        let noted = if running { std::mem::take(&mut self.changes) } else { Vec::new() };
        let lost = self.presence.sync(&self.server.world, &self.atlas, &mut self.bots, &self.ids, &self.by_char, noted, &mut self.rng);
        for (i, wait) in lost {
            self.reschedule(i, wait);
        }
        {
            let presence = &self.presence;
            hire::assist(&mut self.server.world.lock(), &self.hires, |i| presence.shown_at(i));
        }
        // What players did to bots.
        for (i, killer, capsule, type_id) in std::mem::take(&mut self.presence.kills) {
            let side = self.politics.side_of(self.ids[i as usize].corp);
            let enemies: Vec<i32> = side
                .map(|s| self.politics.wars.iter().filter_map(|w| if w.a == s { Some(w.b) } else if w.b == s { Some(w.a) } else { None }).collect())
                .unwrap_or_default();
            let class = ships::weight_class(self.yard.group_of(type_id as i32));
            let b = &self.bots[i as usize];
            let kill = relations::Kill { victim: i, killer: killer as i32, pod: capsule, class, system: b.system };
            self.relations.killed(kill, b.id, side, &enemies, now);
        }
        self.relations.publish(&self.bots);
        relations::act(&mut self.relations, &self.db, &self.server.db_url, &self.server.world).await;
        let news = std::mem::take(&mut self.relations.news);
        self.politics.tell(news);
        if !self.following.bots.is_empty() {
            let (names, econ) = (&self.names, &self.econ);
            self.following.look(&self.bots, |s| names.system(s), |s| names.station(s), |h| econ.goods.get(&h).map_or_else(|| h.to_string(), |g| g.name.clone()));
        }
    }

    /// Requests from the dashboard and `/bots`.
    async fn requests(&mut self, now: u32) {
        for r in control::take() {
            match r {
                control::Request::Event(kind) => {
                    if self.run_events(vec![kind], false, now).await == 0 {
                        self.say(format!("{kind:?} asked for, but nothing called for one: no alliances it could happen to now"));
                    }
                }
                control::Request::War(a, b) => {
                    let (x, y) = (self.find_side(&a).await, self.find_side(&b).await);
                    match (x, y) {
                        (Some(x), Some(y)) if x != y && !self.politics.wars.iter().any(|w| (w.a == x && w.b == y) || (w.a == y && w.b == x)) => {
                            let speed = self.speed;
                            match politics::war_between(&mut self.politics, &self.db, x, y, 7, now, speed).await {
                                Ok(_) => {
                                    crate::svc::war::refresh(&self.db).await;
                                    let at = crate::bulkdata::filetime_now();
                                    self.politics.tell(vec![(at, format!("{a} and {b} are at war"))]);
                                    self.say(format!("War between {a} and {b}"));
                                }
                                Err(e) => error!("bots: a war by request: {}", crate::logging::chain(&e)),
                            }
                        }
                        (Some(_), Some(_)) => self.say(format!("{a} and {b} are already at war (or the same)")),
                        _ => self.say(format!("No bot alliance or corporation called {a} or {b}")),
                    }
                }
                control::Request::Density(region, factor) => {
                    let sql = r#"SELECT "regionID", "regionName" FROM "mapRegions" WHERE lower("regionName") = lower($1) OR "regionID"::text = $1"#;
                    match self.db.query_opt(sql, &[&region]).await {
                        Ok(Some(row)) => {
                            let id: i32 = row.get(0);
                            let name: Option<String> = row.get(1);
                            let saved = if (factor - 1.0).abs() < 1e-3 {
                                self.density.remove(&id);
                                self.db.execute(r#"DELETE FROM "botRegionDensity" WHERE "regionID" = $1"#, &[&id]).await
                            } else {
                                self.density.insert(id, factor);
                                self.db
                                    .execute(r#"INSERT INTO "botRegionDensity" ("regionID","factor") VALUES ($1,$2) ON CONFLICT ("regionID") DO UPDATE SET "factor" = EXCLUDED."factor""#, &[&id, &factor])
                                    .await
                            };
                            if let Err(e) = saved {
                                error!("bots: saving region density: {}", crate::logging::chain(&e));
                            }
                            self.say(format!("{} is now {factor}x as busy as usual", name.unwrap_or(region)));
                        }
                        _ => self.say(format!("No region called {region}")),
                    }
                }
                control::Request::Follow(who) => {
                    let wanted = who.to_lowercase();
                    let id: Option<i64> = who.parse().ok();
                    let found = self
                        .ids
                        .iter()
                        .position(|i| i.name.to_lowercase() == wanted)
                        .or_else(|| self.ids.iter().position(|i| i.name.to_lowercase().starts_with(&wanted)))
                        .or_else(|| id.and_then(|c| self.by_char.get(&c).map(|i| *i as usize)));
                    match found {
                        Some(i) if self.following.follow(i as u32) => {
                            let name = self.ids[i].name.clone();
                            self.say(format!("Following {name}"));
                        }
                        Some(_) => self.say("Following too many pilots already".into()),
                        None => self.say(format!("No bot called {who}")),
                    }
                }
                control::Request::Unfollow(char_id) => {
                    if let Some(&i) = self.by_char.get(&char_id) {
                        self.following.unfollow(i);
                    }
                }
            }
        }
    }

    /// A bot alliance or corporation by name, ticker or ID.
    async fn find_side(&self, name: &str) -> Option<i32> {
        let sql = r#"SELECT "allianceID" FROM alliances WHERE (lower("allianceName") = lower($1) OR lower("shortName") = lower($1) OR "allianceID"::text = $1) AND "deleted" = 0
                     UNION ALL
                     SELECT "corporationID" FROM "corporation" WHERE lower("corporationName") = lower($1) OR lower("tickerName") = lower($1) OR "corporationID"::text = $1"#;
        let rows = self.db.query(sql, &[&name.trim()]).await.ok()?;
        rows.iter().map(|r| r.get::<_, i32>(0)).find(|id| self.politics.alliances.contains_key(id) || self.politics.corps.contains_key(id))
    }

    /// Every few seconds: the changed bots, the economy, relations and kill
    /// reports saved; the dashboard's windows and the map's counts.
    async fn flush_all(&mut self, running: bool) {
        let rows = self.changed.len();
        match flush(&self.db, &self.bots, &self.changed).await {
            Ok(()) => {
                for i in std::mem::take(&mut self.changed) {
                    self.dirty[i as usize] = false;
                    let b = &mut self.bots[i as usize];
                    b.isk_saved = b.isk;
                }
            }
            Err(e) => error!("bots: saving {rows} bot(s): {}", crate::logging::chain(&e)),
        }
        self.tally.flushed_rows += rows as u64;
        if !self.hulls_changed.is_empty() {
            let list = std::mem::take(&mut self.hulls_changed);
            if let Err(e) = save_hulls(&self.db, &self.bots, &self.ids, &list).await {
                error!("bots: saving new ships: {}", crate::logging::chain(&e));
            }
        }
        if let Err(e) = economy::flush(&mut self.econ, &self.db).await {
            error!("bots: saving the economy: {}", crate::logging::chain(&e));
        }
        if let Err(e) = relations::flush(&mut self.relations, &self.db).await {
            error!("bots: saving relations: {}", crate::logging::chain(&e));
        }
        let kills = std::mem::take(&mut self.politics.unseen_kills);
        if let Err(e) = write_kills(&self.db, &kills, &self.bots, &self.ids).await {
            error!("bots: writing kill reports: {}", crate::logging::chain(&e));
        }
        let mut events = panels::events(&self.events, &self.names);
        events["said"] = serde_json::Value::Array(self.said.iter().rev().map(|(t, l)| serde_json::json!({ "time": t / 10_000 - 11_644_473_600_000, "text": l })).collect());
        events["controls"] = serde_json::Value::Bool(controls_enabled());
        let panels = crate::world::BotPanels {
            market: panels::market(&self.econ, &self.names),
            relations: panels::relations(&self.db, &self.relations, &self.hires, &self.bots, &self.ids).await,
            followed: panels::followed(&self.db, &self.following, &self.bots, &self.ids, &self.econ, &self.atlas, &self.names).await,
            events,
        };
        self.server.world.lock().bot_panels = panels;
        publish_counts(&self.server, &self.bots, &self.politics, &self.sieges, running);
    }
}

/// Writes back where the changed bots are, whether they play, what their
/// wallets gained or lost, and their homes, in one batch.
async fn flush(db: &Client, bots: &[Bot], changed: &[u32]) -> Result<(), tokio_postgres::Error> {
    if changed.is_empty() {
        return Ok(());
    }
    let pick = |f: &dyn Fn(&Bot) -> i32| changed.iter().map(|i| f(&bots[*i as usize])).collect::<Vec<i32>>();
    let ids = pick(&|b| b.id);
    let stations = pick(&|b| b.station);
    let systems = pick(&|b| b.system);
    let homes = pick(&|b| b.home);
    let online: Vec<i16> = changed.iter().map(|i| i16::from(bots[*i as usize].online)).collect();
    // A change, not a value: what players paid them meanwhile stays.
    let delta: Vec<f64> = changed.iter().map(|i| bots[*i as usize].isk - bots[*i as usize].isk_saved).collect();
    let sql = r#"UPDATE "character_" c SET "stationID" = u.station, "solarSystemID" = u.system,
                        "constellationID" = s."constellationID", "regionID" = s."regionID", "online" = u.online, "balance" = c."balance" + u.delta
                   FROM unnest($1::int[], $2::int[], $3::int[], $4::smallint[], $5::float8[]) AS u(id, station, system, online, delta)
                   JOIN "mapSolarSystems" s ON s."solarSystemID" = u.system
                  WHERE c."characterID" = u.id"#;
    db.execute(sql, &[&ids, &stations, &systems, &online, &delta]).await?;
    let sql = r#"UPDATE "bots" b SET "homeStationID" = u.home FROM unnest($1::int[], $2::int[]) AS u(id, home)
                  WHERE b."characterID" = u.id AND b."homeStationID" <> u.home"#;
    db.execute(sql, &[&ids, &homes]).await?;
    Ok(())
}

/// The bots' new ships: their ship items take the new hull, and their tier
/// is kept.
async fn save_hulls(db: &Client, bots: &[Bot], ids: &[Identity], list: &[u32]) -> Result<(), tokio_postgres::Error> {
    let ships: Vec<i32> = list.iter().map(|i| ids[*i as usize].ship as i32).collect();
    let hulls: Vec<i32> = list.iter().map(|i| bots[*i as usize].hull).collect();
    db.execute(r#"UPDATE entity e SET "typeID" = u.hull FROM unnest($1::int[], $2::int[]) AS u(id, hull) WHERE e."itemID" = u.id"#, &[&ships, &hulls]).await?;
    let chars: Vec<i32> = list.iter().map(|i| bots[*i as usize].id).collect();
    let tiers: Vec<i16> = list.iter().map(|i| bots[*i as usize].tier as i16).collect();
    db.execute(r#"UPDATE "bots" b SET "tier" = u.tier FROM unnest($1::int[], $2::smallint[]) AS u(id, tier) WHERE b."characterID" = u.id"#, &[&chars, &tiers]).await?;
    Ok(())
}

/// What players paid the bots (and the market filled) since the wallets were
/// last read: into each bot's wallet in memory.
async fn reload_balances(db: &Client, bots: &mut [Bot], by_char: &HashMap<i64, u32>) -> Result<(), tokio_postgres::Error> {
    let rows = db.query(r#"SELECT c."characterID", c."balance" FROM "bots" b JOIN "character_" c ON c."characterID" = b."characterID""#, &[]).await?;
    for r in rows {
        let Some(&i) = by_char.get(&(r.get::<_, i32>(0) as i64)) else { continue };
        let balance: f64 = r.get(1);
        let b = &mut bots[i as usize];
        let outside = balance - b.isk_saved;
        if outside.abs() >= 0.01 {
            b.isk += outside;
            b.isk_saved = balance;
        }
    }
    Ok(())
}

/// Kill reports for the fights nobody saw: one line each in the kill log,
/// with the bot that laid the final blow.
async fn write_kills(db: &Client, kills: &[(u32, u32, i32, i32)], bots: &[Bot], ids: &[Identity]) -> Result<(), tokio_postgres::Error> {
    if kills.is_empty() {
        return Ok(());
    }
    let numbers: Vec<i64> = db
        .query(r#"SELECT nextval(pg_get_serial_sequence('"killLog"', 'killID')) FROM generate_series(1, $1)"#, &[&(kills.len() as i32)])
        .await?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let col = |f: &dyn Fn(&(u32, u32, i32, i32)) -> i32| kills.iter().map(f).collect::<Vec<i32>>();
    let victims = col(&|k| bots[k.0 as usize].id);
    let victim_corps = col(&|k| ids[k.0 as usize].corp);
    let victim_alliances = col(&|k| ids[k.0 as usize].alliance.unwrap_or(0));
    let lost = col(&|k| k.3);
    let items = col(&|k| ids[k.0 as usize].ship as i32);
    let systems = col(&|k| k.2);
    let killers = col(&|k| bots[k.1 as usize].id);
    let killer_corps = col(&|k| ids[k.1 as usize].corp);
    let killer_alliances = col(&|k| ids[k.1 as usize].alliance.unwrap_or(0));
    let killer_ships = col(&|k| bots[k.1 as usize].hull);
    let now = crate::bulkdata::filetime_now();
    db.execute(
        r#"INSERT INTO "killLog" ("killID","killTime","victimCharID","victimCorpID","victimAllianceID","victimShipTypeID",
                                  "victimItemID","solarSystemID","locationID","finalBlowID","totalDamage","isPodKill")
           SELECT u.id, $8::bigint, u.victim, u.corp, u.alliance, u.lost, u.item, u.system, u.system, u.killer, 1000, 0
             FROM unnest($1::bigint[], $2::int[], $3::int[], $4::int[], $5::int[], $6::int[], $7::int[], $9::int[])
                  AS u(id, victim, corp, alliance, lost, item, system, killer)"#,
        &[&numbers, &victims, &victim_corps, &victim_alliances, &lost, &items, &systems, &now, &killers],
    )
    .await?;
    db.execute(
        r#"INSERT INTO "killLogParticipants" ("killID","ownerID","corporationID","allianceID","shipTypeID","weaponTypeID","damageDone","finalBlow")
           SELECT u.id, u.who, u.corp, u.alliance, u.ship, 0, 1000, 1
             FROM unnest($1::bigint[], $2::int[], $3::int[], $4::int[], $5::int[]) AS u(id, who, corp, alliance, ship)"#,
        &[&numbers, &killers, &killer_corps, &killer_alliances, &killer_ships],
    )
    .await?;
    Ok(())
}

/// The star map's counts take in the online bots: (in space, docked) by
/// system; and the dashboard's figures follow.
fn publish_counts(server: &Server, bots: &[Bot], politics: &politics::Politics, sieges: &[sov::Siege], running: bool) {
    let mut counts: HashMap<i64, (i64, i64)> = HashMap::new();
    if running {
        for b in bots.iter().filter(|b| b.online) {
            let c = counts.entry(b.system as i64).or_default();
            if b.station == 0 { c.0 += 1 } else { c.1 += 1 }
        }
    }
    let (online, in_space) = counts.values().fold((0, 0), |(o, s), (sp, dk)| (o + sp + dk, s + sp));
    let stats = crate::world::BotStats {
        total: bots.len() as i64,
        online,
        in_space,
        corporations: politics.corps.len() as i64,
        alliances: politics.alliances.len() as i64,
        wars: politics.wars.len() as i64,
        lost: politics.lost,
        war_losses: politics.wars.iter().map(|w| (w.id as i64, w.kills)).collect(),
        news: politics.news.iter().cloned().collect(),
        sieges: sieges
            .iter()
            .map(|s| crate::world::SiegeView {
                attacker: s.names.attacker.clone(),
                defender: s.names.defender.clone(),
                system: s.names.system.clone(),
                region: s.names.region.clone(),
                security: s.names.security,
                starts: s.starts_at,
                fleets: s.fight.as_ref().map(|f| {
                    let (a, d) = f.standing(bots, s.system);
                    (a, d)
                }),
            })
            .collect(),
        running,
    };
    let mut local: HashMap<i64, Vec<u32>> = HashMap::new();
    if running {
        for (i, b) in bots.iter().enumerate().filter(|(_, b)| b.online) {
            local.entry(b.system as i64).or_default().push(i as u32);
        }
    }
    let mut w = server.world.lock();
    w.bot_counts = counts;
    w.bot_stats = stats;
    let before = std::mem::replace(&mut w.bot_local, local);
    // The first time, everyone is already there: nobody arrives.
    if !before.is_empty() {
        tell_local(&w, &before);
    }
}

/// Tells everyone with a Local channel open which bots came and went there
/// since `before`.
fn tell_local(w: &crate::world::WorldState, before: &HashMap<i64, Vec<u32>>) {
    use crate::svc::chat;
    for (system, channel, members) in w.local_channels() {
        let was: std::collections::HashSet<u32> = before.get(&system).map(|v| v.iter().copied().collect()).unwrap_or_default();
        let now: std::collections::HashSet<u32> = w.bot_local.get(&system).map(|v| v.iter().copied().collect()).unwrap_or_default();
        let count = members.len() + now.len();
        let came = now.difference(&was).map(|i| (*i, "JoinChannel"));
        let went = was.difference(&now).map(|i| (*i, "LeaveChannel"));
        for (i, method) in came.chain(went) {
            if let Some(b) = w.bot_roster.get(i as usize) {
                chat::announce_counted(w, &members, count, &channel, method, &chat::bot_who(b), Vec::new());
            }
        }
    }
}

/// Every bot as Local shows it: name, corporation, alliance, portrait type.
fn roster(bots: &[Bot], ids: &[Identity], types: &HashMap<i32, i64>) -> std::sync::Arc<Vec<crate::world::LocalBot>> {
    let list = bots
        .iter()
        .zip(ids)
        .map(|(b, id)| crate::world::LocalBot {
            char_id: b.id as i64,
            corp_id: id.corp as i64,
            alliance_id: id.alliance.map(i64::from),
            name: id.name.clone(),
            type_id: types.get(&b.id).copied().unwrap_or(1373),
        })
        .collect();
    std::sync::Arc::new(list)
}

/// The character type (by bloodline) of every bot, for their portraits in Local.
async fn character_types(db: &Client) -> HashMap<i32, i64> {
    let sql = r#"SELECT c."characterID", c."typeID" FROM "bots" b JOIN "character_" c ON c."characterID" = b."characterID""#;
    match db.query(sql, &[]).await {
        Ok(rows) => rows.iter().map(|r| (r.get::<_, i32>(0), r.get::<_, i32>(1) as i64)).collect(),
        Err(e) => {
            error!("bots: reading their character types: {}", crate::logging::chain(&e));
            HashMap::new()
        }
    }
}

fn report(bots: &[Bot], t: &Tally, over: Duration, clock: f64, speed: f64, running: bool) -> String {
    let online = bots.iter().filter(|b| b.online).count();
    let in_space = bots.iter().filter(|b| b.online && b.station == 0).count();
    let mut roles: HashMap<&str, usize> = HashMap::new();
    for b in bots.iter().filter(|b| b.online) {
        *roles.entry(b.role.name()).or_default() += 1;
    }
    let mut roles: Vec<String> = roles.into_iter().map(|(r, n)| format!("{r} {n}")).collect();
    roles.sort();
    let secs = over.as_secs_f64().max(0.001);
    format!(
        "{}{} bots, {online} online ({in_space} in space; {}); {:.0} steps/s, {:.2} ms stepping per second, worst tick {:.2} ms; \
         {} save(s) of {:.0} rows on average, {:.0} ms on average, worst {:.0} ms; bot clock {:.1} h at {speed}x",
        if running { "" } else { "PAUSED: " },
        bots.len(),
        roles.join(", "),
        t.steps as f64 / secs,
        t.step_time.as_secs_f64() * 1_000.0 / secs,
        t.worst_tick.as_secs_f64() * 1_000.0,
        t.flushes,
        t.flushed_rows as f64 / t.flushes.max(1) as f64,
        t.flush_time.as_secs_f64() * 1_000.0 / t.flushes.max(1) as f64,
        t.worst_flush.as_secs_f64() * 1_000.0,
        clock / 3_600.0,
    )
}
