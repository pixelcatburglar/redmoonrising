//! State shared by every connection: who is in the game, which chat channels
//! they have open, the solar-system ballparks, and a queue per connection for
//! notifications (and session moves) the rest of the server sends it.
//!
//! One lock guards it all, so a change that touches several players (a ship
//! leaving a system, a chat line) is seen by everyone in the same order. Nothing
//! awaits while holding it: notifications are queued, not sent.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use eve_marshal::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::ballpark::Park;

/// A notification for one client: `event(*args)` on its side.
#[derive(Debug, Clone)]
pub struct Notification {
    pub event: &'static str,
    /// The positional arguments, as a tuple.
    pub args: Value,
}

/// Something that happened to a character which its connection has to carry
/// into its session, because the client did not ask for it.
#[derive(Debug, Clone, PartialEq)]
pub enum Move {
    /// The ship was destroyed; the pilot is now in `ship_id`, a capsule, in
    /// the same system.
    ShipLost { ship_id: i64 },
    /// The capsule was destroyed; the pilot wakes in a clone at a station, in a
    /// new capsule if one could be made.
    Podded { station_id: i64, solar_system_id: i64, constellation_id: i64, region_id: i64, ship_id: Option<i64> },
    /// The character's corporation, its alliance or its roles changed; the
    /// connection reads them again.
    Affiliation,
    /// The character joined, left or now leads (or no longer leads) a gang.
    Gang { gang_id: Option<i64>, leader: bool },
    /// A GM's /tr: into a station, or to a point in a system's space (only
    /// from space).
    Teleport { station_id: Option<i64>, solar_system_id: i64, constellation_id: i64, region_id: i64, pos: (f64, f64, f64) },
}

/// A call the server makes on one of the client's own services; the answer
/// (None if the client raised, or went away) comes back on `reply`.
#[derive(Debug)]
pub struct ClientCall {
    pub service: &'static str,
    pub method: &'static str,
    pub args: Vec<Value>,
    pub reply: tokio::sync::oneshot::Sender<Option<Value>>,
}

/// What a connection's queue carries, in the order it was queued.
#[derive(Debug)]
pub enum Push {
    Notify(Notification),
    Move(Move),
    Call(ClientCall),
}

/// Where one connected character is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presence {
    pub char_id: i64,
    pub corp_id: i64,
    pub solar_system_id: i64,
    pub docked: bool,
    /// The station, when docked.
    pub station_id: Option<i64>,
    /// The session's role mask.
    pub role: i64,
}

struct Client {
    tx: UnboundedSender<Push>,
    presence: Option<Presence>,
}

struct Channel {
    /// The channel ID as the client knows it.
    id: Value,
    members: HashSet<i64>,
}

#[derive(Default)]
pub struct WorldState {
    clients: HashMap<i64, Client>,
    /// Open chat channels, by `channel_key`.
    channels: HashMap<String, Channel>,
    /// Player trades under way, by trade container.
    pub trades: HashMap<i64, crate::svc::pvptrade::Trade>,
    /// Solar system -> its ballpark. Created when the first ship arrives.
    pub parks: HashMap<i64, Park>,
    pub gangs: crate::svc::gang::Gangs,
    /// The online bots by solar system: (in space, docked), for the star map.
    pub bot_counts: HashMap<i64, (i64, i64)>,
    /// How the bot population stands, for the web dashboard.
    pub bot_stats: BotStats,
}

/// The bot population at its last save.
#[derive(Debug, Clone, Default)]
pub struct BotStats {
    pub total: i64,
    pub online: i64,
    pub in_space: i64,
    pub corporations: i64,
    pub alliances: i64,
    pub wars: i64,
    /// Bots lost in fights since the server started.
    pub lost: u64,
    /// Bots lost in each bot war, by warID.
    pub war_losses: HashMap<i64, u32>,
    /// What happened in null-sec lately, oldest first: (FILETIME, what).
    pub news: Vec<(i64, String)>,
    pub running: bool,
}

/// A handle on the shared world; clones share it, for tasks that outlive a call.
#[derive(Default, Clone)]
pub struct World {
    state: Arc<Mutex<WorldState>>,
}

impl World {
    pub fn lock(&self) -> MutexGuard<'_, WorldState> {
        self.state.lock().expect("world lock poisoned")
    }
}

impl WorldState {
    /// Registers a connection; notifications for it arrive on the receiver.
    pub fn connect(&mut self, client_id: i64) -> UnboundedReceiver<Push> {
        let (tx, rx) = unbounded_channel();
        self.clients.insert(client_id, Client { tx, presence: None });
        rx
    }

    /// Forgets a connection and its chat memberships; returns each channel it
    /// was in with the members left, for the caller to announce the departure.
    pub fn disconnect(&mut self, client_id: i64) -> Vec<(Value, Vec<i64>)> {
        if let Some(char_id) = self.clients.get(&client_id).and_then(|c| c.presence).map(|p| p.char_id) {
            crate::svc::gang::logged_off(self, char_id);
        }
        self.clients.remove(&client_id);
        let mut left = Vec::new();
        for channel in self.channels.values_mut() {
            if channel.members.remove(&client_id) {
                left.push((channel.id.clone(), channel.members.iter().copied().collect()));
            }
        }
        self.channels.retain(|_, c| !c.members.is_empty());
        left
    }

    pub fn set_presence(&mut self, client_id: i64, presence: Option<Presence>) {
        if let Some(c) = self.clients.get_mut(&client_id) {
            c.presence = presence;
        }
    }


    /// The connections playing `char_id` (at most one, in practice).
    pub fn clients_of(&self, char_id: i64) -> Vec<i64> {
        self.clients
            .iter()
            .filter(|(_, c)| c.presence.is_some_and(|p| p.char_id == char_id))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Where each player in the game is.
    pub fn players(&self) -> Vec<Presence> {
        self.clients.values().filter_map(|c| c.presence).collect()
    }

    /// Characters in the game: players logged in, and the bots online.
    pub fn online_count(&self) -> i64 {
        let players = self.clients.values().filter(|c| c.presence.is_some()).count() as i64;
        let bots: i64 = self.bot_counts.values().map(|(space, docked)| space + docked).sum();
        players + bots
    }

    /// Clients whose session holds every bit of `role`.
    pub fn with_role(&self, role: i64) -> Vec<i64> {
        self.clients.iter().filter(|(_, c)| c.presence.is_some_and(|p| p.role & role == role)).map(|(id, _)| *id).collect()
    }

    /// Where `char_id` is, if online.
    pub fn presence_of(&self, char_id: i64) -> Option<Presence> {
        self.clients.values().filter_map(|c| c.presence).find(|p| p.char_id == char_id)
    }

    /// The station `char_id` is docked in, if online and docked.
    pub fn station_of(&self, char_id: i64) -> Option<i64> {
        self.clients.values().filter_map(|c| c.presence).find(|p| p.char_id == char_id).and_then(|p| p.station_id)
    }

    pub fn is_online(&self, char_id: i64) -> bool {
        self.clients.values().any(|c| c.presence.is_some_and(|p| p.char_id == char_id))
    }

    /// Whether `char_id` is online and undocked.
    pub fn is_in_space(&self, char_id: i64) -> bool {
        self.clients.values().any(|c| c.presence.is_some_and(|p| p.char_id == char_id && !p.docked))
    }

    /// Characters per solar system: (in space, docked). Every occupied system has
    /// an in-space entry, if only 0: the star map iterates that dict's keys alone.
    pub fn counts(&self) -> (HashMap<i64, i64>, HashMap<i64, i64>) {
        let (mut space, mut docked) = (HashMap::new(), HashMap::new());
        for p in self.clients.values().filter_map(|c| c.presence) {
            let in_space = space.entry(p.solar_system_id).or_insert(0);
            if p.docked {
                *docked.entry(p.solar_system_id).or_insert(0) += 1;
            } else {
                *in_space += 1;
            }
        }
        for (system, (in_space, in_station)) in &self.bot_counts {
            *space.entry(*system).or_insert(0) += in_space;
            if *in_station > 0 {
                *docked.entry(*system).or_insert(0) += in_station;
            }
        }
        (space, docked)
    }

    /// Clients whose character belongs to `corp_id`.
    pub fn corp_members(&self, corp_id: i64) -> Vec<i64> {
        self.clients
            .iter()
            .filter(|(_, c)| c.presence.is_some_and(|p| p.corp_id == corp_id))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Clients whose character is in space in `solar_system_id`.
    pub fn in_space(&self, solar_system_id: i64) -> Vec<i64> {
        self.clients
            .iter()
            .filter(|(_, c)| {
                c.presence.is_some_and(|p| p.solar_system_id == solar_system_id && !p.docked)
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// Queues `event(*args)` for each of `clients`. A client that has gone away
    /// is skipped.
    pub fn notify(&self, clients: &[i64], event: &'static str, args: Value) {
        for id in clients {
            if let Some(c) = self.clients.get(id) {
                let _ = c.tx.send(Push::Notify(Notification { event, args: args.clone() }));
            }
        }
    }

    /// Calls `service.method(*args)` on a client; the receiver yields its
    /// answer. None if the client is not connected.
    pub fn call(&self, client_id: i64, service: &'static str, method: &'static str, args: Vec<Value>) -> Option<tokio::sync::oneshot::Receiver<Option<Value>>> {
        let c = self.clients.get(&client_id)?;
        let (reply, answer) = tokio::sync::oneshot::channel();
        c.tx.send(Push::Call(ClientCall { service, method, args, reply })).ok()?;
        Some(answer)
    }

    /// Queues a session move for one client, after anything already queued.
    pub fn push_move(&self, client_id: i64, m: Move) {
        if let Some(c) = self.clients.get(&client_id) {
            let _ = c.tx.send(Push::Move(m));
        }
    }

    /// Tells the connections of everyone in `corp_ids`, and of `char_ids`,
    /// to read their corporation and alliance again.
    pub fn reaffiliate(&self, corp_ids: &[i64], char_ids: &[i64]) {
        let mut clients: Vec<i64> = corp_ids.iter().flat_map(|c| self.corp_members(*c)).collect();
        clients.extend(char_ids.iter().flat_map(|c| self.clients_of(*c)));
        clients.sort_unstable();
        clients.dedup();
        for client in clients {
            self.push_move(client, Move::Affiliation);
        }
    }

    /// Opens a chat channel for a client; returns everyone who has it open now.
    pub fn join_channel(&mut self, channel_id: &Value, client_id: i64) -> Vec<i64> {
        let channel = self
            .channels
            .entry(channel_key(channel_id))
            .or_insert_with(|| Channel { id: channel_id.clone(), members: HashSet::new() });
        channel.members.insert(client_id);
        channel.members.iter().copied().collect()
    }

    /// Closes a chat channel for a client; returns who still has it open.
    pub fn leave_channel(&mut self, channel_id: &Value, client_id: i64) -> Vec<i64> {
        let key = channel_key(channel_id);
        let Some(channel) = self.channels.get_mut(&key) else { return Vec::new() };
        channel.members.remove(&client_id);
        let rest = channel.members.iter().copied().collect();
        if channel.members.is_empty() {
            self.channels.remove(&key);
        }
        rest
    }

    /// Everyone who has a chat channel open.
    pub fn channel_members(&self, channel_id: &Value) -> Vec<i64> {
        self.channels
            .get(&channel_key(channel_id))
            .map(|c| c.members.iter().copied().collect())
            .unwrap_or_default()
    }
}

/// A chat channel's identity as a map key. Channel IDs are ints for ordinary
/// channels and tuples like `(('solarsystemid2', 30002780),)` for local chat.
fn channel_key(channel_id: &Value) -> String {
    format!("{channel_id:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn presence(char_id: i64, docked: bool) -> Option<Presence> {
        Some(Presence { char_id, corp_id: 1000044, solar_system_id: 30002780, docked, station_id: None, role: 3 })
    }

    #[test]
    fn tracks_characters_until_they_leave() {
        let world = World::default();
        let mut w = world.lock();
        let _a = w.connect(1);
        let _b = w.connect(2);
        w.set_presence(1, presence(140000001, true));
        w.set_presence(2, presence(140000002, false));
        assert!(w.is_online(140000001));
        assert_eq!(w.in_space(30002780), vec![2]);
        let (space, docked) = w.counts();
        assert_eq!((space[&30002780], docked[&30002780]), (1, 1));

        w.disconnect(2);
        let (space, docked) = w.counts();
        assert_eq!((space[&30002780], docked[&30002780]), (0, 1), "docked-only system still listed");

        w.disconnect(1);
        assert!(!w.is_online(140000001));
        assert!(w.counts().0.is_empty());
    }

    #[test]
    fn notifications_reach_only_the_named_clients() {
        let world = World::default();
        let mut w = world.lock();
        let mut a = w.connect(1);
        let mut b = w.connect(2);
        w.notify(&[2], "OnTest", Value::Tuple(vec![Value::Int(7)]));
        assert!(a.try_recv().is_err());
        let Push::Notify(n) = b.try_recv().expect("queued for client 2") else { panic!("a notification") };
        assert_eq!(n.event, "OnTest");
    }

    #[test]
    fn channel_membership_follows_joins_leaves_and_disconnects() {
        let world = World::default();
        let mut w = world.lock();
        let _a = w.connect(1);
        let _b = w.connect(2);
        let channel = Value::Int(2000000000);
        w.join_channel(&channel, 1);
        let mut members = w.join_channel(&channel, 2);
        members.sort();
        assert_eq!(members, vec![1, 2]);
        assert_eq!(w.leave_channel(&channel, 1), vec![2]);
        assert_eq!(w.disconnect(2), vec![(channel.clone(), Vec::new())]);
        assert!(w.channel_members(&channel).is_empty());
    }
}
