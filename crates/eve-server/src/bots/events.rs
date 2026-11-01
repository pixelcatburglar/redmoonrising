//! What shakes bot politics up now and then, beyond wars and sieges:
//!
//! - **Betrayal**: a corporation of a null-sec alliance at war goes over to
//!   the enemy, and the towers in the systems its pilots worked go with it.
//! - **Merger**: two small neighbouring null-sec alliances at peace become
//!   one, the smaller folding into the bigger, towers and all.
//! - **Collapse**: a null-sec alliance with no towers left falls apart; its
//!   corporations go their own way.
//! - **Coalition**: an alliance holding far more space than any other finds
//!   its neighbours declaring war on it together.
//! - **Raid**: a pirate alliance sends a fleet into a high-sec system near
//!   its space for a couple of hours. The raiders attack players there on
//!   sight (CONCORD answers them as it would anyone) and pick off the bots
//!   they find.
//!
//! Each happens by chance on the politics tick, or when asked for from the
//! dashboard or `/bots event`.

use std::collections::{HashMap, HashSet};

use tokio_postgres::Client;
use tracing::info;

use super::behaviour::{self, Activity, Atlas, Bot, Role};
use super::politics::{Politics, KIND_NULL_ALLIANCE, KIND_PIRATE_ALLIANCE};
use super::presence::{Identity, Presence};
use super::sov::News;
use crate::combat::Rng;
use crate::svc::rows::int;

/// Each event's chance on a politics tick.
const BETRAYAL_CHANCE: f64 = 0.004;
const MERGER_CHANCE: f64 = 0.003;
const COLLAPSE_CHANCE: f64 = 0.01;
const COALITION_CHANCE: f64 = 0.01;
const RAID_CHANCE: f64 = 0.006;
/// A raid's size and length (bots' clock).
const RAIDERS: (usize, usize) = (15, 45);
const RAID_SECS: u32 = 2 * 3_600;
const DAY: u32 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Betrayal,
    Merger,
    Collapse,
    Coalition,
    Raid,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s.to_ascii_lowercase().as_str() {
            "betrayal" | "betray" => Some(Kind::Betrayal),
            "merger" | "merge" => Some(Kind::Merger),
            "collapse" => Some(Kind::Collapse),
            "coalition" => Some(Kind::Coalition),
            "raid" | "invasion" | "invade" => Some(Kind::Raid),
            _ => None,
        }
    }
}

/// A pirate raid under way.
pub struct Raid {
    pub alliance: i32,
    pub system: i32,
    pub until: u32,
    pub raiders: Vec<u32>,
    pub kills: u32,
    pub lost: u32,
    pub name: String,
}

#[derive(Default)]
pub struct Events {
    pub raids: Vec<Raid>,
    /// When a coalition last formed, against whom.
    coalition_until: HashMap<i32, u32>,
    pub log: Vec<(i64, String)>,
}

impl Events {
    fn tell(&mut self, news: &mut Vec<News>, text: String) {
        let at = crate::bulkdata::filetime_now();
        info!("bots: {text}");
        self.log.push((at, text.clone()));
        if self.log.len() > 40 {
            self.log.remove(0);
        }
        news.push((at, text));
    }
}

/// Every politics tick: maybe an event, and the ones asked for.
#[allow(clippy::too_many_arguments)]
pub async fn tick(
    ev: &mut Events,
    db: &Client,
    atlas: &Atlas,
    p: &mut Politics,
    bots: &mut [Bot],
    ids: &mut [Identity],
    presence: &mut Presence,
    asked: Vec<Kind>,
    roll: bool,
    rng: &mut Rng,
    clock: u32,
) -> Result<(Vec<News>, Vec<(u32, u32)>), tokio_postgres::Error> {
    let mut news = Vec::new();
    let mut moved = Vec::new();
    let mut due: Vec<Kind> = asked;
    for (kind, chance) in [
        (Kind::Betrayal, BETRAYAL_CHANCE),
        (Kind::Merger, MERGER_CHANCE),
        (Kind::Collapse, COLLAPSE_CHANCE),
        (Kind::Coalition, COALITION_CHANCE),
        (Kind::Raid, RAID_CHANCE),
    ] {
        if roll && rng.unit() < chance {
            due.push(kind);
        }
    }
    for kind in due {
        match kind {
            Kind::Betrayal => betrayal(ev, db, p, ids, rng, &mut news).await?,
            Kind::Merger => merger(ev, db, p, ids, rng, &mut news).await?,
            Kind::Collapse => collapse(ev, db, p, ids, &mut news).await?,
            Kind::Coalition => coalition(ev, db, p, rng, clock, &mut news).await?,
            Kind::Raid => moved.extend(raid(ev, db, atlas, p, bots, ids, presence, rng, clock, &mut news).await?),
        }
    }
    Ok((news, moved))
}

/// Systems an alliance holds towers in.
async fn holdings(db: &Client) -> Result<HashMap<i32, HashSet<i32>>, tokio_postgres::Error> {
    let sql = r#"SELECT COALESCE(co."allianceID", 0), e."locationID" FROM "posStructures" p JOIN entity e ON e."itemID" = p."itemID"
                   JOIN "corporation" co ON co."corporationID" = e."ownerID" WHERE p."claim" = 1"#;
    let mut out: HashMap<i32, HashSet<i32>> = HashMap::new();
    for r in db.query(sql, &[]).await? {
        out.entry(int(&r, 0).unwrap_or(0) as i32).or_default().insert(int(&r, 1).unwrap_or(0) as i32);
    }
    Ok(out)
}

async fn executor(db: &Client, alliance: i32) -> Option<i32> {
    db.query_opt(r#"SELECT "executorCorpID" FROM alliances WHERE "allianceID" = $1"#, &[&alliance]).await.ok().flatten().and_then(|r| int(&r, 0)).map(|v| v as i32)
}

/// Moves a corporation into an alliance (0: out of any), in the database
/// and in memory.
async fn move_corp(db: &Client, p: &mut Politics, ids: &mut [Identity], corp: i32, to: i32) -> Result<(), tokio_postgres::Error> {
    let from = p.corps.get(&corp).and_then(|c| c.alliance).unwrap_or(0);
    let now = crate::bulkdata::filetime_now();
    db.execute(r#"UPDATE "corporation" SET "allianceID" = $2 WHERE "corporationID" = $1"#, &[&corp, &to]).await?;
    db.execute(r#"DELETE FROM "allianceMembers" WHERE "corporationID" = $1"#, &[&corp]).await?;
    if to != 0 {
        let exec = executor(db, to).await.unwrap_or(corp);
        db.execute(r#"INSERT INTO "allianceMembers" ("allianceID","corporationID","chosenExecutorID","startDate") VALUES ($1,$2,$3,$4)"#, &[&to, &corp, &exec, &now])
            .await?;
        db.execute(r#"UPDATE alliances SET "memberCount" = "memberCount" + 1 WHERE "allianceID" = $1"#, &[&to]).await?;
    }
    if from != 0 {
        db.execute(r#"UPDATE alliances SET "memberCount" = GREATEST(0, "memberCount" - 1) WHERE "allianceID" = $1"#, &[&from]).await?;
    }
    let alliance = (to != 0).then_some(to);
    if let Some(c) = p.corps.get_mut(&corp) {
        c.alliance = alliance;
    }
    for id in ids.iter_mut().filter(|id| id.corp == corp) {
        id.alliance = alliance;
    }
    Ok(())
}

/// Hands an alliance's towers in `systems` to another's executor.
async fn hand_over(db: &Client, from: i32, to_exec: i32, systems: &[i32]) -> Result<u64, tokio_postgres::Error> {
    db.execute(
        r#"UPDATE entity e SET "ownerID" = $3 FROM "corporation" co
            WHERE co."corporationID" = e."ownerID" AND co."allianceID" = $1 AND e."locationID" = ANY($2)
              AND (EXISTS (SELECT 1 FROM "posStructures" p WHERE p."itemID" = e."itemID")
                   OR EXISTS (SELECT 1 FROM "posStructures" p WHERE p."itemID" = e."locationID"))"#,
        &[&from, &systems, &to_exec],
    )
    .await
}

async fn end_wars_of(db: &Client, p: &mut Politics, alliance: i32) -> Result<(), tokio_postgres::Error> {
    let now = crate::bulkdata::filetime_now();
    let ended: Vec<i32> = p.wars.iter().filter(|w| w.a == alliance || w.b == alliance).map(|w| w.id).collect();
    for id in &ended {
        db.execute(r#"UPDATE "crpWars" SET "retracted" = $2, "timeFinished" = $2 WHERE "warID" = $1"#, &[id, &now]).await?;
    }
    p.wars.retain(|w| w.a != alliance && w.b != alliance);
    if !ended.is_empty() {
        crate::svc::war::refresh(db).await;
    }
    Ok(())
}

async fn betrayal(ev: &mut Events, db: &Client, p: &mut Politics, ids: &mut [Identity], rng: &mut Rng, news: &mut Vec<News>) -> Result<(), tokio_postgres::Error> {
    let wars: Vec<(i32, i32)> = p
        .wars
        .iter()
        .filter(|w| [w.a, w.b].iter().all(|s| p.alliances.get(s).is_some_and(|a| a.kind == KIND_NULL_ALLIANCE)))
        .flat_map(|w| [(w.a, w.b), (w.b, w.a)])
        .collect();
    let Some(&(traitor_of, to)) = wars.get(rng.below(wars.len().max(1))) else { return Ok(()) };
    let exec = executor(db, traitor_of).await;
    let mut members: Vec<i32> = p.corps.iter().filter(|(id, c)| c.alliance == Some(traitor_of) && Some(**id) != exec).map(|(id, _)| *id).collect();
    members.sort_unstable();
    if members.len() < 2 {
        return Ok(());
    }
    let corp = members[rng.below(members.len())];
    // The systems its pilots worked.
    let turf: Vec<i32> = Vec::new();
    let held = holdings(db).await?;
    let theirs = held.get(&traitor_of).cloned().unwrap_or_default();
    let share = (theirs.len() / (members.len() + 1)).max(1);
    let mut systems: Vec<i32> = theirs.into_iter().collect();
    systems.sort_unstable();
    systems.truncate(share);
    let _ = turf;
    move_corp(db, p, ids, corp, to).await?;
    let taken = match executor(db, to).await {
        Some(to_exec) => hand_over(db, traitor_of, to_exec, &systems).await?,
        None => 0,
    };
    let (who, from, into) = (super::sov::alliance_name(db, corp).await, super::sov::alliance_name(db, traitor_of).await, super::sov::alliance_name(db, to).await);
    let who = corp_name(db, corp).await.unwrap_or(who);
    ev.tell(news, format!("Betrayal: {who} left {from} for their enemies {into}{}", if taken > 0 { format!(", taking {} system(s) with them", systems.len()) } else { String::new() }));
    Ok(())
}

async fn corp_name(db: &Client, corp: i32) -> Option<String> {
    db.query_opt(r#"SELECT "corporationName" FROM "corporation" WHERE "corporationID" = $1"#, &[&corp]).await.ok().flatten().and_then(|r| r.get(0))
}

fn pilots_of(p: &Politics, alliance: i32) -> u32 {
    p.corps.values().filter(|c| c.alliance == Some(alliance)).map(|c| c.members).sum()
}

async fn merger(ev: &mut Events, db: &Client, p: &mut Politics, ids: &mut [Identity], rng: &mut Rng, news: &mut Vec<News>) -> Result<(), tokio_postgres::Error> {
    let null: Vec<(i32, i32)> = p.alliances.iter().filter(|(_, a)| a.kind == KIND_NULL_ALLIANCE).filter_map(|(id, a)| a.region.map(|r| (*id, r))).collect();
    let mut sizes: Vec<u32> = null.iter().map(|(a, _)| pilots_of(p, *a)).collect();
    sizes.sort_unstable();
    let median = sizes.get(sizes.len() / 2).copied().unwrap_or(0);
    let at_war = |a: i32, b: i32| p.wars.iter().any(|w| (w.a == a && w.b == b) || (w.a == b && w.b == a));
    let mut pairs = Vec::new();
    for (x, (a, ra)) in null.iter().enumerate() {
        for (b, rb) in &null[x + 1..] {
            if p.neighbours_of(*ra).contains(rb) && !at_war(*a, *b) && pilots_of(p, *a) <= median && pilots_of(p, *b) <= median {
                pairs.push((*a, *b));
            }
        }
    }
    pairs.sort_unstable();
    let Some(&(a, b)) = pairs.get(rng.below(pairs.len().max(1))) else { return Ok(()) };
    let (big, small) = if pilots_of(p, a) >= pilots_of(p, b) { (a, b) } else { (b, a) };
    let corps: Vec<i32> = p.corps.iter().filter(|(_, c)| c.alliance == Some(small)).map(|(id, _)| *id).collect();
    if let Some(exec) = executor(db, big).await {
        let systems: Vec<i32> = holdings(db).await?.get(&small).map(|s| s.iter().copied().collect()).unwrap_or_default();
        hand_over(db, small, exec, &systems).await?;
    }
    for corp in corps {
        move_corp(db, p, ids, corp, big).await?;
    }
    end_wars_of(db, p, small).await?;
    let (bn, sn) = (super::sov::alliance_name(db, big).await, super::sov::alliance_name(db, small).await);
    dissolve(db, p, small).await?;
    ev.tell(news, format!("Merger: {sn} has joined {bn}"));
    Ok(())
}

/// Ends an alliance: deleted, no longer the bots'.
async fn dissolve(db: &Client, p: &mut Politics, alliance: i32) -> Result<(), tokio_postgres::Error> {
    db.execute(r#"UPDATE alliances SET "deleted" = 1, "memberCount" = 0 WHERE "allianceID" = $1"#, &[&alliance]).await?;
    db.execute(r#"DELETE FROM "botGroups" WHERE "ownerID" = $1"#, &[&alliance]).await?;
    db.execute(r#"DELETE FROM "allianceMembers" WHERE "allianceID" = $1"#, &[&alliance]).await?;
    p.alliances.remove(&alliance);
    Ok(())
}

async fn collapse(ev: &mut Events, db: &Client, p: &mut Politics, ids: &mut [Identity], news: &mut Vec<News>) -> Result<(), tokio_postgres::Error> {
    let held = holdings(db).await?;
    let mut broken: Vec<i32> = p
        .alliances
        .iter()
        .filter(|(id, a)| a.kind == KIND_NULL_ALLIANCE && held.get(id).is_none_or(|s| s.is_empty()) && p.treasury.get(id).copied().unwrap_or(0.0) < super::sov::TOWER_COST)
        .map(|(id, _)| *id)
        .collect();
    broken.sort_unstable();
    let Some(&alliance) = broken.first() else { return Ok(()) };
    let corps: Vec<i32> = p.corps.iter().filter(|(_, c)| c.alliance == Some(alliance)).map(|(id, _)| *id).collect();
    for corp in &corps {
        move_corp(db, p, ids, *corp, 0).await?;
    }
    end_wars_of(db, p, alliance).await?;
    let name = super::sov::alliance_name(db, alliance).await;
    dissolve(db, p, alliance).await?;
    ev.tell(news, format!("Collapse: {name}, its space and its money gone, has broken apart; its {} corporations go their own way", corps.len()));
    Ok(())
}

async fn coalition(ev: &mut Events, db: &Client, p: &mut Politics, rng: &mut Rng, clock: u32, news: &mut Vec<News>) -> Result<(), tokio_postgres::Error> {
    let held = holdings(db).await?;
    let mut sizes: Vec<(i32, usize)> = held.iter().filter(|(a, _)| p.alliances.get(a).is_some_and(|x| x.kind == KIND_NULL_ALLIANCE)).map(|(a, s)| (*a, s.len())).collect();
    sizes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let (Some(&(top, n)), second) = (sizes.first(), sizes.get(1).map_or(0, |s| s.1)) else { return Ok(()) };
    if n < 20 || (n as f64) < 1.8 * second as f64 || ev.coalition_until.get(&top).is_some_and(|t| *t > clock) {
        return Ok(());
    }
    let at_war = |p: &Politics, a: i32, b: i32| p.wars.iter().any(|w| (w.a == a && w.b == b) || (w.a == b && w.b == a));
    let mut joining: Vec<i32> = sizes.iter().skip(1).map(|(a, _)| *a).filter(|a| !at_war(p, *a, top)).take(3).collect();
    joining.sort_unstable();
    if joining.is_empty() {
        return Ok(());
    }
    let speed = p.speed;
    for &a in &joining {
        let days = 7 + rng.below(8) as u32;
        super::politics::war_between(p, db, a, top, days, clock, speed).await?;
    }
    crate::svc::war::refresh(db).await;
    ev.coalition_until.insert(top, clock + 7 * DAY);
    let mut names = Vec::new();
    for a in &joining {
        names.push(super::sov::alliance_name(db, *a).await);
    }
    let target = super::sov::alliance_name(db, top).await;
    ev.tell(news, format!("Coalition: {} declare war together on {target}, which holds {n} systems", names.join(", ")));
    Ok(())
}

/// A pirate alliance raids a high-sec system near its space.
#[allow(clippy::too_many_arguments)]
async fn raid(
    ev: &mut Events,
    db: &Client,
    atlas: &Atlas,
    p: &Politics,
    bots: &mut [Bot],
    ids: &[Identity],
    presence: &mut Presence,
    rng: &mut Rng,
    clock: u32,
    news: &mut Vec<News>,
) -> Result<Vec<(u32, u32)>, tokio_postgres::Error> {
    let mut pirates: Vec<(i32, i32)> = p.alliances.iter().filter(|(_, a)| a.kind == KIND_PIRATE_ALLIANCE).filter_map(|(id, a)| a.region.map(|r| (*id, r))).collect();
    pirates.retain(|(a, _)| !ev.raids.iter().any(|r| r.alliance == *a));
    pirates.sort_unstable();
    let Some(&(alliance, region)) = pirates.get(rng.below(pirates.len().max(1))) else { return Ok(Vec::new()) };
    // A high-sec system (0.5 to 0.7) within a few jumps of the region.
    let start = atlas.systems.iter().filter(|(_, s)| s.region == region).map(|(id, _)| *id).min();
    let Some(start) = start else { return Ok(Vec::new()) };
    let mut targets: Vec<i32> = crate::gates::reach_on(atlas.graph, start as i64, 8)
        .into_iter()
        .map(|(s, _)| s as i32)
        .filter(|s| atlas.systems.get(s).is_some_and(|x| (0.5..0.75).contains(&x.security)))
        .collect();
    targets.sort_unstable();
    let Some(&system) = targets.get(rng.below(targets.len().max(1))) else { return Ok(Vec::new()) };
    let want = RAIDERS.0 + rng.below(RAIDERS.1 - RAIDERS.0 + 1);
    let mut fit: Vec<u32> = (0..bots.len() as u32)
        .filter(|i| {
            let (b, id) = (&bots[*i as usize], &ids[*i as usize]);
            id.alliance == Some(alliance) && b.role == Role::Pirate && b.pod.is_none() && !matches!(b.activity, Activity::Sieging | Activity::Escorting) && presence.armed(id.hull)
        })
        .collect();
    fit.sort_by_key(|i| u8::from(!bots[*i as usize].online));
    fit.truncate(want);
    if fit.len() < 5 {
        return Ok(Vec::new());
    }
    let mut moved = Vec::new();
    for &i in &fit {
        let b = &mut bots[i as usize];
        b.online = true;
        b.session_end = b.session_end.max(clock + RAID_SECS);
        let wait = behaviour::go(b, atlas, system, 0, rng);
        moved.push((i, wait));
        presence.raiders.insert(i);
    }
    let name = super::sov::system_name(db, system).await;
    let who = super::sov::alliance_name(db, alliance).await;
    ev.tell(news, format!("Raid: {} pirates of {who} are on their way to {name}, in high-sec", fit.len()));
    ev.raids.push(Raid { alliance, system, until: clock + RAID_SECS, raiders: fit, kills: 0, lost: 0, name });
    Ok(moved)
}

/// Every minute of the bots' clock: raiders in their target system, where
/// nobody watches, pick off the bots there (and some fall to CONCORD); raids
/// that are over end, and the raiders go home. Returns the bots whose state
/// changed, with the seconds to their next step.
pub fn raids(ev: &mut Events, atlas: &Atlas, p: &mut Politics, bots: &mut [Bot], presence: &mut Presence, rng: &mut Rng, clock: u32) -> (Vec<(u32, u32)>, Vec<News>) {
    let mut moved = Vec::new();
    let mut news = Vec::new();
    let watched = presence.watched().clone();
    for r in ev.raids.iter_mut() {
        let here: Vec<u32> = r.raiders.iter().copied().filter(|i| bots[*i as usize].system == r.system && bots[*i as usize].station == 0 && bots[*i as usize].pod.is_none()).collect();
        // Arrived raiders camp the gates.
        for &i in &here {
            if bots[i as usize].activity != Activity::Camping {
                bots[i as usize].activity = Activity::Camping;
            }
        }
        if here.is_empty() || watched.contains(&r.system) {
            continue;
        }
        let victims: Vec<u32> = (0..bots.len() as u32)
            .filter(|i| {
                let b = &bots[*i as usize];
                b.online && b.system == r.system && b.station == 0 && b.role != Role::Pirate && b.activity != Activity::Escorting
            })
            .collect();
        if let Some(&v) = victims.get(rng.below(victims.len().max(1))).filter(|_| rng.unit() < (0.1 * here.len() as f64).min(0.9)) {
            let killer = here[rng.below(here.len())];
            p.unseen_kills.push((v, killer, r.system, bots[v as usize].hull));
            let wait = behaviour::reclone(&mut bots[v as usize], atlas, rng);
            moved.push((v, wait));
            r.kills += 1;
            p.lost += 1;
        }
        // CONCORD takes its toll of criminals.
        if rng.unit() < 0.15 {
            let i = here[rng.below(here.len())];
            let wait = behaviour::reclone(&mut bots[i as usize], atlas, rng);
            moved.push((i, wait));
            r.lost += 1;
            p.lost += 1;
        }
    }
    let (over, kept): (Vec<Raid>, Vec<Raid>) = std::mem::take(&mut ev.raids).into_iter().partition(|r| r.until <= clock);
    ev.raids = kept;
    for r in over {
        for &i in &r.raiders {
            presence.raiders.remove(&i);
            let b = &mut bots[i as usize];
            if b.station == 0 && b.pod.is_none() && b.activity != Activity::Sieging {
                moved.push((i, behaviour::release(b, atlas, rng)));
            }
        }
        let text = format!("The raid on {} is over: the pirates killed {} and lost {}", r.name, r.kills, r.lost);
        let at = crate::bulkdata::filetime_now();
        ev.log.push((at, text.clone()));
        news.push((at, text));
    }
    (moved, news)
}
