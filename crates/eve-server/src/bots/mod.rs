//! The bot population: characters the server plays, so the universe is not
//! empty. `RMR_BOTS` sets how many there are (made on first start, kept
//! after); `RMR_BOTS_SPEED` runs their clock faster, for testing; `/bots
//! on|off|status` pauses and resumes them while the server runs.
//!
//! Each bot is stepped (`behaviour::step`) when its next moment comes up,
//! out of one queue ordered by time. Their state lives here; every few
//! seconds what changed is written back in one batch, and the star map's
//! pilot counts take in where the online ones are. Where a player is in
//! space, the bots there are ships (`presence`). Most belong to bot
//! corporations and alliances, which fight wars among themselves
//! (`politics`) and hold null-sec with control towers (`sov`). Bots do not
//! yet trade on the market or join player corporations.

pub mod behaviour;
mod politics;
mod population;
mod presence;
mod sov;

pub use presence::eject;

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio_postgres::Client;
use tracing::{error, info, warn};

use crate::combat::Rng;
use crate::conn::Server;
use behaviour::{Atlas, Bot};
use presence::{Identity, Presence};

/// How often the clock moves and due bots are stepped.
const TICK: Duration = Duration::from_millis(250);
/// How often what changed is saved and the map's counts refreshed.
const FLUSH: Duration = Duration::from_secs(10);
/// How often, on the bots' clock, politics moves and enemies in the same
/// place may fight.
const POLITICS_EVERY: u32 = 600;
const SKIRMISH_EVERY: u32 = 60;
/// How often space is brought up to date with the bots.
const SYNC: Duration = Duration::from_secs(1);
/// How often the run is summed up in the log.
const REPORT: Duration = Duration::from_secs(60);

static RUNNING: AtomicBool = AtomicBool::new(true);
static STATUS: Mutex<String> = Mutex::new(String::new());

/// /bots on|off.
pub fn set_running(on: bool) {
    RUNNING.store(on, Ordering::Relaxed);
}

/// /bots status: the last summary.
pub fn status() -> String {
    let s = STATUS.lock().map(|s| s.clone()).unwrap_or_default();
    if s.is_empty() { "The bots are not running.".into() } else { s }
}

pub async fn run(server: Arc<Server>, target: usize, speed: f64) {
    if target == 0 {
        return;
    }
    match start(&server, target).await {
        Ok((mut db, atlas, mut bots, mut ids)) => {
            let presence = Presence::new(presence::hulls(&db, &ids).await);
            let mut rng = Rng::seeded(crate::bulkdata::filetime_now() as u64 ^ 0x5eed);
            let politics = match politics::load(&db, &atlas, 0, speed).await {
                Ok(p) if !p.corps.is_empty() => Ok(p),
                Ok(_) => politics::form(&mut db, &atlas, &mut bots, &mut ids, &mut rng, 0, speed).await,
                Err(e) => Err(e),
            };
            match politics {
                Ok(mut politics) => {
                    match sov::establish(&db, &atlas, &politics, &mut rng, server.sov_claim_delay_secs).await {
                        Ok(news) if !news.is_empty() => {
                            politics.tell(news);
                            crate::svc::starbase::sovereignty(&db, server.sov_claim_delay_secs).await;
                        }
                        Ok(_) => {}
                        Err(e) => error!("bots: settling null-sec: {}", crate::logging::chain(&e)),
                    }
                    simulate(server, db, atlas, bots, ids, presence, politics, speed).await
                }
                Err(e) => error!("bots: politics: {}", crate::logging::chain(&e)),
            }
        }
        Err(e) => error!("bots: {e}"),
    }
}

async fn start(server: &Server, target: usize) -> Result<(Client, Atlas, Vec<Bot>, Vec<Identity>), String> {
    let mut db = crate::db::connect(&server.db_url).await.map_err(|e| crate::logging::chain(&e).to_string())?;
    let started = Instant::now();
    let atlas = population::atlas(&db).await?;
    info!("bots: map of {} systems, {} stations read in {:.1}s", atlas.systems.len(), atlas.station_system.len(), started.elapsed().as_secs_f64());
    let have = population::ensure(&mut db, &atlas, target).await?;
    let (bots, ids) = population::load(&db, &atlas, target).await.map_err(|e| crate::logging::chain(&e).to_string())?;
    info!("bots: {} loaded of {have}, in {:.1}s", bots.len(), started.elapsed().as_secs_f64());
    Ok((db, atlas, bots, ids))
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

async fn simulate(
    server: Arc<Server>,
    db: Client,
    atlas: Atlas,
    mut bots: Vec<Bot>,
    mut ids: Vec<Identity>,
    mut presence: Presence,
    mut politics: politics::Politics,
    speed: f64,
) {
    let (mut next_politics, mut next_skirmish) = (POLITICS_EVERY, SKIRMISH_EVERY);
    let by_char: HashMap<i64, u32> = bots.iter().enumerate().map(|(i, b)| (b.id as i64, i as u32)).collect();
    let mut changes = Vec::new();
    let mut last_sync = Instant::now();
    let mut rng = Rng::seeded(crate::bulkdata::filetime_now() as u64);
    // The bots' own clock, in seconds; it only moves while they run.
    let mut clock = 0.0f64;
    // (when, bot, generation): a bot rescheduled out of turn gets a new
    // generation, and its old entry is passed over.
    let mut due: BinaryHeap<Reverse<(u32, u32, u32)>> = BinaryHeap::with_capacity(bots.len());
    let mut generation = vec![0u32; bots.len()];
    for (i, bot) in bots.iter_mut().enumerate() {
        let wait = behaviour::wake_up(bot, 0, &mut rng);
        due.push(Reverse((wait, i as u32, 0)));
    }
    let mut dirty = vec![true; bots.len()];
    let mut changed: Vec<u32> = (0..bots.len() as u32).collect();
    let mut tally = Tally::default();
    let (mut last_flush, mut last_report, mut last_tick) = (Instant::now(), Instant::now(), Instant::now());
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    info!("bots: running {} at {speed}x", bots.len());
    loop {
        interval.tick().await;
        let elapsed = last_tick.elapsed();
        last_tick = Instant::now();
        let running = RUNNING.load(Ordering::Relaxed);
        if running {
            clock += elapsed.as_secs_f64() * speed;
            let now = clock as u32;
            let work = Instant::now();
            while let Some(&Reverse((at, i, gen))) = due.peek() {
                if at > now {
                    break;
                }
                due.pop();
                if gen != generation[i as usize] {
                    continue;
                }
                let bot = &mut bots[i as usize];
                let before = (bot.system, bot.station, bot.activity);
                let wait = behaviour::step(bot, now, &atlas, &mut rng).max(1);
                presence.note(i, before, bot, &mut changes);
                due.push(Reverse((now + wait, i, gen)));
                tally.steps += 1;
                if !dirty[i as usize] {
                    dirty[i as usize] = true;
                    changed.push(i);
                }
            }
            if now >= next_skirmish {
                next_skirmish = now + SKIRMISH_EVERY;
                for (i, wait) in politics::skirmish(&mut politics, &atlas, &mut bots, &ids, presence.watched(), &mut rng) {
                    generation[i as usize] += 1;
                    due.push(Reverse((now + wait, i, generation[i as usize])));
                    if !dirty[i as usize] {
                        dirty[i as usize] = true;
                        changed.push(i);
                    }
                }
            }
            let took = work.elapsed();
            tally.step_time += took;
            tally.worst_tick = tally.worst_tick.max(took);
            if now >= next_politics {
                next_politics = now + POLITICS_EVERY;
                if let Err(e) = politics::tick(&mut politics, &db, &bots, &mut ids, &mut rng, now, speed).await {
                    error!("bots: politics: {}", crate::logging::chain(&e));
                }
                match sov::upkeep(&db, &atlas, &mut politics, presence.watched(), &mut rng, server.sov_claim_delay_secs, speed).await {
                    Ok(news) => politics.tell(news),
                    Err(e) => error!("bots: sovereignty: {}", crate::logging::chain(&e)),
                }
            }
        }
        if last_sync.elapsed() >= SYNC {
            last_sync = Instant::now();
            let noted = if running { std::mem::take(&mut changes) } else { Vec::new() };
            for (i, wait) in presence.sync(&server.world, &atlas, &mut bots, &ids, &by_char, noted, &mut rng) {
                generation[i as usize] += 1;
                due.push(Reverse((clock as u32 + wait, i, generation[i as usize])));
                if !dirty[i as usize] {
                    dirty[i as usize] = true;
                    changed.push(i);
                }
            }
        }
        if last_flush.elapsed() >= FLUSH {
            last_flush = Instant::now();
            let rows = changed.len();
            match flush(&db, &bots, &changed).await {
                Ok(()) => {
                    for i in changed.drain(..) {
                        dirty[i as usize] = false;
                    }
                }
                Err(e) => error!("bots: saving {rows} bot(s): {}", crate::logging::chain(&e)),
            }
            let took = last_flush.elapsed();
            tally.flushes += 1;
            tally.flushed_rows += rows as u64;
            tally.flush_time += took;
            tally.worst_flush = tally.worst_flush.max(took);
            publish_counts(&server, &bots, &politics, running);
        }
        if last_report.elapsed() >= REPORT {
            let mut summary = report(&bots, &tally, last_report.elapsed(), clock, speed, running);
            summary.push_str(&format!("; {} bot ship(s) shown in {} watched system(s); {}", presence.shown(), presence.observed(), politics.summary()));
            politics.kills = 0;
            info!("bots: {summary}");
            if let Ok(mut s) = STATUS.lock() {
                *s = summary;
            }
            if tally.worst_flush > FLUSH / 2 {
                warn!("bots: a save took {:.1}s, half the time between saves", tally.worst_flush.as_secs_f64());
            }
            tally = Tally::default();
            last_report = Instant::now();
        }
    }
}

/// Writes back where the changed bots are, whether they play, their money
/// and their homes, in one batch.
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
    let isk: Vec<f64> = changed.iter().map(|i| bots[*i as usize].isk).collect();
    let sql = r#"UPDATE "character_" c SET "stationID" = u.station, "solarSystemID" = u.system,
                        "constellationID" = s."constellationID", "regionID" = s."regionID", "online" = u.online, "balance" = u.isk
                   FROM unnest($1::int[], $2::int[], $3::int[], $4::smallint[], $5::float8[]) AS u(id, station, system, online, isk)
                   JOIN "mapSolarSystems" s ON s."solarSystemID" = u.system
                  WHERE c."characterID" = u.id"#;
    db.execute(sql, &[&ids, &stations, &systems, &online, &isk]).await?;
    let sql = r#"UPDATE "bots" b SET "homeStationID" = u.home FROM unnest($1::int[], $2::int[]) AS u(id, home)
                  WHERE b."characterID" = u.id AND b."homeStationID" <> u.home"#;
    db.execute(sql, &[&ids, &homes]).await?;
    Ok(())
}

/// The star map's counts take in the online bots: (in space, docked) by
/// system; and the dashboard's figures follow.
fn publish_counts(server: &Server, bots: &[Bot], politics: &politics::Politics, running: bool) {
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
        running,
    };
    let mut w = server.world.lock();
    w.bot_counts = counts;
    w.bot_stats = stats;
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
