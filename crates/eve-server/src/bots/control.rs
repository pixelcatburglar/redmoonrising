//! Steering the bots by hand, from the web dashboard or `/bots`: start an
//! event or a war, make a region busier or quieter, and follow a pilot to
//! see what it does. Requests queue here and the simulation takes them up on
//! its next tick.
//!
//! Following a bot keeps a short history of what it did (where it went,
//! what it earned, the ships it lost and bought), shown on the dashboard.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use super::behaviour::{Activity, Bot};
use super::events;

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Event(events::Kind),
    /// Two alliances, by name (or ID), to go to war.
    War(String, String),
    /// A region, by name (or ID), and how much busier than usual (1 is usual).
    Density(String, f32),
    /// A bot, by name (or character ID).
    Follow(String),
    Unfollow(i64),
}

static QUEUE: Mutex<Vec<Request>> = Mutex::new(Vec::new());

pub fn ask(r: Request) {
    if let Ok(mut q) = QUEUE.lock() {
        q.push(r);
    }
}

pub fn take() -> Vec<Request> {
    QUEUE.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
}

/// Parses a dashboard or `/bots` request: `event raid`, `war A | B`,
/// `density Region 1.5`, `follow Name`, `unfollow 140000001`.
pub fn parse(words: &[&str]) -> Result<Request, String> {
    let rest = words.get(1..).unwrap_or(&[]).join(" ");
    match words.first().map(|w| w.to_ascii_lowercase()).as_deref() {
        Some("event") => events::Kind::parse(rest.trim()).map(Request::Event).ok_or_else(|| "Events: betrayal, merger, collapse, coalition, raid.".to_string()),
        Some("war") => {
            let (a, b) = rest.split_once('|').ok_or("war <alliance> | <alliance>")?;
            Ok(Request::War(a.trim().to_string(), b.trim().to_string()))
        }
        Some("density") => {
            let (region, factor) = rest.trim().rsplit_once(' ').ok_or("density <region> <factor>")?;
            let factor: f32 = factor.parse().map_err(|_| "The factor is a number: 1 is usual, 2 twice as busy, 0.5 half.")?;
            Ok(Request::Density(region.trim().to_string(), factor.clamp(0.0, 5.0)))
        }
        Some("follow") if !rest.trim().is_empty() => Ok(Request::Follow(rest.trim().to_string())),
        Some("unfollow") => rest.trim().parse().map(Request::Unfollow).map_err(|_| "unfollow <characterID>".to_string()),
        _ => Err("event <kind> | war <a> | <b> | density <region> <factor> | follow <name> | unfollow <id>".into()),
    }
}

/// The most bots followed at once, and the history kept of each.
const FOLLOWED: usize = 12;
const HISTORY: usize = 60;

/// What a followed bot was, the last time it was looked at.
#[derive(Clone, Copy, PartialEq)]
pub struct Seen {
    system: i32,
    station: i32,
    activity: Activity,
    online: bool,
    hull: i32,
    pod: bool,
}

#[derive(Default)]
pub struct Following {
    pub bots: HashMap<u32, (Option<Seen>, VecDeque<(i64, String)>, f64)>,
}

impl Following {
    pub fn follow(&mut self, i: u32) -> bool {
        if self.bots.contains_key(&i) {
            return true;
        }
        if self.bots.len() >= FOLLOWED {
            return false;
        }
        self.bots.insert(i, (None, VecDeque::new(), f64::NAN));
        true
    }

    pub fn unfollow(&mut self, i: u32) {
        self.bots.remove(&i);
    }

    /// Notes what changed in each followed bot since the last look.
    pub fn look(&mut self, bots: &[Bot], system_name: impl Fn(i32) -> String, station_name: impl Fn(i32) -> String, hull_name: impl Fn(i32) -> String) {
        let now = crate::bulkdata::filetime_now();
        for (i, (last, history, isk)) in self.bots.iter_mut() {
            let b = &bots[*i as usize];
            let seen = Seen { system: b.system, station: b.station, activity: b.activity, online: b.online, hull: b.hull, pod: b.pod.is_some() };
            let mut lines = Vec::new();
            match last {
                None => lines.push(format!("Followed: {} in {}", b.activity.name(), system_name(b.system))),
                Some(was) if *was != seen => {
                    if was.online != seen.online {
                        lines.push(if seen.online { "Logged on".into() } else { "Logged off".into() });
                    }
                    if !was.pod && seen.pod {
                        lines.push(format!("Ship destroyed in {}; in a capsule", system_name(b.system)));
                    }
                    if was.hull != seen.hull {
                        lines.push(format!("Now flies a {}", hull_name(seen.hull)));
                    }
                    if was.station != seen.station && seen.station != 0 {
                        lines.push(format!("Docked at {}", station_name(seen.station)));
                    } else if was.station != 0 && seen.station == 0 {
                        lines.push(format!("Undocked from {}", station_name(was.station)));
                    }
                    if was.system != seen.system && seen.station == 0 {
                        lines.push(format!("Jumped to {}", system_name(seen.system)));
                    }
                    if was.activity != seen.activity && !matches!(seen.activity, Activity::Docked | Activity::Travelling) {
                        lines.push(format!("Now {} in {}", seen.activity.name(), system_name(b.system)));
                    }
                }
                _ => {}
            }
            if !isk.is_nan() && (b.isk - *isk).abs() >= 100_000.0 {
                let d = b.isk - *isk;
                lines.push(format!("{} {:.1}M ISK", if d > 0.0 { "Earned" } else { "Spent" }, d.abs() / 1.0e6));
            }
            *isk = b.isk;
            *last = Some(seen);
            for line in lines {
                history.push_back((now, line));
            }
            while history.len() > HISTORY {
                history.pop_front();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_parse_from_words() {
        assert_eq!(parse(&["event", "raid"]), Ok(Request::Event(events::Kind::Raid)));
        assert_eq!(parse(&["war", "Black", "Legion", "|", "Red", "Pact"]), Ok(Request::War("Black Legion".into(), "Red Pact".into())));
        assert_eq!(parse(&["density", "The", "Forge", "2"]), Ok(Request::Density("The Forge".into(), 2.0)));
        assert_eq!(parse(&["follow", "Ana", "Sen"]), Ok(Request::Follow("Ana Sen".into())));
        assert!(parse(&["event", "picnic"]).is_err());
    }

    #[test]
    fn a_followed_bot_s_moves_are_written_down() {
        let mut f = Following::default();
        let mut bots = vec![super::super::behaviour::tests::bot(super::super::behaviour::Role::Miner)];
        f.follow(0);
        let name = |n: i32| n.to_string();
        f.look(&bots, name, name, name);
        bots[0].station = 0;
        bots[0].system = 2;
        bots[0].activity = Activity::Mining;
        bots[0].isk = 5_000_000.0;
        f.look(&bots, name, name, name);
        let lines: Vec<&str> = f.bots[&0].1.iter().map(|(_, l)| l.as_str()).collect();
        assert!(lines.iter().any(|l| l.starts_with("Undocked")), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("Now mining in 2")), "{lines:?}");
        assert!(lines.iter().any(|l| l.starts_with("Earned 5.0M")), "{lines:?}");
    }
}
