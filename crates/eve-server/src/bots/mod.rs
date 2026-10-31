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
mod battle;
mod defense;
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

pub async fn run(server: Arc<Server>, target: usize, speed: f64, shown: usize) {
    if target == 0 {
        return;
    }
    match start(&server, target).await {
        Ok((mut db, atlas, mut bots, mut ids)) => {
            let presence = Presence::new(presence::hulls(&db, &ids).await, shown);
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
                    let types = character_types(&db).await;
                    simulate(server, db, atlas, bots, ids, types, presence, politics, speed).await
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
    types: HashMap<i32, i64>,
    mut presence: Presence,
    mut politics: politics::Politics,
    speed: f64,
) {
    let (mut next_politics, mut next_skirmish) = (POLITICS_EVERY, SKIRMISH_EVERY);
    // Sieges announced, and those being fought.
    let mut sieges: Vec<sov::Siege> = Vec::new();
    // Towers under attack, and the fleets defending them.
    let mut defenses: Vec<defense::Defense> = Vec::new();
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
    server.world.lock().bot_roster = roster(&bots, &ids, &types);
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
                // Some joined corporations: Local shows them in their new ones.
                let fresh = roster(&bots, &ids, &types);
                server.world.lock().bot_roster = fresh;
                match sov::upkeep(&db, &atlas, &mut politics, presence.watched(), &mut sieges, &mut rng, now, server.sov_claim_delay_secs, speed).await {
                    Ok(news) => politics.tell(news),
                    Err(e) => error!("bots: sovereignty: {}", crate::logging::chain(&e)),
                }
            }
        }
        if last_sync.elapsed() >= SYNC {
            last_sync = Instant::now();
            if running {
                let now = clock as u32;
                let mut report = defense::run(&mut defenses, &server.world, &db, &atlas, &mut bots, &ids, &mut presence, &politics, &mut rng, now).await;
                if !sieges.is_empty() {
                    let fought = battle::run(&mut sieges, &server.world, &db, &atlas, &mut bots, &ids, &mut presence, &mut politics, &mut rng, now, server.sov_claim_delay_secs, speed).await;
                    match fought {
                        Ok(r) => report.merge(r),
                        Err(e) => error!("bots: sieges: {}", crate::logging::chain(&e)),
                    }
                }
                politics.tell(report.news);
                for i in report.touched {
                    if !dirty[i as usize] {
                        dirty[i as usize] = true;
                        changed.push(i);
                    }
                }
                // Sent home: they set off now.
                for i in report.released {
                    generation[i as usize] += 1;
                    due.push(Reverse((now + 30, i, generation[i as usize])));
                    changes.push(presence::Change::Busy { bot: i });
                }
            }
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
            publish_counts(&server, &bots, &politics, &sieges, running);
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
