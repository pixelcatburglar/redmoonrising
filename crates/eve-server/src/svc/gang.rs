//! `gangsta`: gangs. A gang lives in memory while its members are online:
//! one leader (or none, for a member to claim), its members, and the pilots
//! invited. Joining, leaving and leading change the member's session
//! (gangid, gangrole), through `world::Move::Gang`; the others hear
//! OnGangJoin, OnGangLeave and OnGangLeader (parklife/gang.py), and pilots
//! in the same system OnCharGangChange (michelle.py:774).
//!
//! An invitation arrives as OnGangInvite(leaderID, gangID, msgName,
//! msgDict) (lscengine.py:1345), the message warning of the wars the gang's
//! members are in. The inviter is warned first of the wars the invitee would
//! bring (GangCandidateDodgy1/N) and asks again with ignoreWars.
//!
//! Choices the client leaves open: only the leader invites, kicks, disbands
//! and hands over (a gang without a leader lets any member invite); a member
//! who logs off leaves the gang; the last member leaving ends it.

use std::collections::HashMap;

use eve_marshal::Value;
use tokio_postgres::GenericClient;
use tracing::info;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::membership::{message, Refused};
use crate::svc::rows::{int, text};
use crate::world::{Move, WorldState};

/// const.gangLeaderRole; members are 0.
pub const GANG_LEADER_ROLE: i64 = 1;
/// The state every member is listed with.
const ACTIVE: &str = "Active";

#[derive(Debug, Default)]
pub struct Gangs {
    next_id: i64,
    gangs: HashMap<i64, Gang>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Gang {
    pub leader: Option<i64>,
    /// In the order they joined.
    pub members: Vec<i64>,
    pub invited: Vec<i64>,
}

impl Gang {
    /// The leader, if they are still a member.
    fn sitting_leader(&self) -> Option<i64> {
        self.leader.filter(|l| self.members.contains(l))
    }
}

/// Who is left after a member goes, and whether they led.
#[derive(Debug, PartialEq)]
struct Departure {
    rest: Vec<i64>,
    was_leader: bool,
}

impl Gangs {
    /// The gang `char_id` is a member of.
    pub fn of(&self, char_id: i64) -> Option<i64> {
        self.gangs.iter().find(|(_, g)| g.members.contains(&char_id)).map(|(id, _)| *id)
    }

    pub fn get(&self, gang_id: i64) -> Option<&Gang> {
        self.gangs.get(&gang_id)
    }

    /// (charID, gangID) for each of `char_ids` in a gang.
    pub fn memberships(&self, char_ids: impl IntoIterator<Item = i64>) -> Vec<(i64, i64)> {
        char_ids.into_iter().filter_map(|c| self.of(c).map(|g| (c, g))).collect()
    }

    fn form(&mut self, leader: i64) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        self.gangs.insert(id, Gang { leader: Some(leader), members: vec![leader], invited: Vec::new() });
        id
    }

    fn join(&mut self, gang_id: i64, char_id: i64) -> bool {
        let Some(g) = self.gangs.get_mut(&gang_id) else { return false };
        if !g.invited.contains(&char_id) {
            return false;
        }
        g.invited.retain(|c| *c != char_id);
        if !g.members.contains(&char_id) {
            g.members.push(char_id);
        }
        true
    }

    fn depart(&mut self, gang_id: i64, char_id: i64) -> Option<Departure> {
        let g = self.gangs.get_mut(&gang_id)?;
        if !g.members.contains(&char_id) {
            return None;
        }
        g.members.retain(|c| *c != char_id);
        let was_leader = g.leader == Some(char_id);
        if was_leader {
            g.leader = None;
        }
        let rest = g.members.clone();
        if rest.is_empty() {
            self.gangs.remove(&gang_id);
        }
        Some(Departure { rest, was_leader })
    }
}

pub async fn gangsta(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let me = ctx.session.char_id.unwrap_or(0);
    let result = match call.method.as_str() {
        "GetGangInfoForChar" => Ok(info_for(&ctx.world.lock(), me)),
        "InviteCharacter" => invite(ctx, me, call.arg_i64(0).unwrap_or(0), call.arg_i64(1).unwrap_or(0) != 0).await,
        "AcceptInvitation" => accept(ctx, me, call.arg_i64(0).unwrap_or(0)),
        "RejectInvitation" => {
            let gang_id = call.arg_i64(0).unwrap_or(0);
            if let Some(g) = ctx.world.lock().gangs.gangs.get_mut(&gang_id) {
                g.invited.retain(|c| *c != me);
            }
            info!("{me} turned down gang {gang_id}");
            Ok(Value::None)
        }
        "LeaveGang" => leave(&mut ctx.world.lock(), me).map(|_| Value::None),
        "KickMember" => kick(&mut ctx.world.lock(), me, call.arg_i64(0).unwrap_or(0)),
        "DisbandGang" => disband(&mut ctx.world.lock(), me),
        "MakeLeader" => make_leader(&mut ctx.world.lock(), me, call.arg_i64(0).unwrap_or(0)),
        "ResignLeadership" => resign(&mut ctx.world.lock(), me),
        "ClaimLeadership" => claim(&mut ctx.world.lock(), me),
        _ => return None,
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

/// (gangID, leaderID, [(charID, state)], invited): gang.py's __Initialize.
fn info_for(w: &WorldState, me: i64) -> Value {
    let Some((gang_id, g)) = w.gangs.of(me).and_then(|id| w.gangs.get(id).map(|g| (id, g))) else {
        return Value::Tuple(vec![Value::None, Value::None, Value::List(Vec::new()), Value::List(Vec::new())]);
    };
    let members = g.members.iter().map(|c| Value::Tuple(vec![Value::Int(*c), Value::text(ACTIVE)])).collect();
    Value::Tuple(vec![
        Value::Int(gang_id),
        Value::int_or_none(g.sitting_leader()),
        Value::List(members),
        Value::List(g.invited.iter().copied().map(Value::Int).collect()),
    ])
}

/// InviteCharacter(charID, ignoreWars=0): forms a gang around the caller if
/// they have none, and invites the pilot.
async fn invite(ctx: &Ctx<'_>, me: i64, invitee: i64, ignore_wars: bool) -> Refused<Value> {
    if ctx.session.station_id.is_some() {
        return Err(message("gangNoInviteInStation"));
    }
    if invitee == me || invitee == 0 {
        return Err(Outcome::custom("You cannot invite yourself."));
    }
    let (online, mine) = {
        let w = ctx.world.lock();
        let mine = w.gangs.of(me).and_then(|id| w.gangs.get(id).map(|g| (id, g.clone())));
        (w.is_online(invitee), mine)
    };
    if !online {
        return Err(message_with("GangCandidateOffline", &[("invitee", Value::text(name_of(ctx.db, invitee).await))]));
    }
    if let Some((_, g)) = &mine {
        if g.sitting_leader().is_some_and(|l| l != me) {
            return Err(message("gangMustBeLeader"));
        }
        if g.members.contains(&invitee) {
            return Err(message("gangMemberAlreadyInGroup"));
        }
        if g.invited.contains(&invitee) {
            return Err(message("gangMemberAlreadyInvited"));
        }
    }

    // The wars on either side, as the sides they are against.
    let members = mine.as_ref().map_or_else(|| vec![me], |(_, g)| g.members.clone());
    let mut member_sides = Vec::new();
    for m in &members {
        member_sides.push(side_of(ctx.db, *m).await);
    }
    let invitee_side = side_of(ctx.db, invitee).await;
    let gang_enemies: Vec<i64> = dedup(member_sides.iter().flat_map(|s| crate::svc::war::enemies_of(*s)).collect());
    let brought: Vec<i64> = crate::svc::war::enemies_of(invitee_side).into_iter().filter(|e| !gang_enemies.contains(e)).collect();
    if !ignore_wars && !brought.is_empty() {
        let names = names_of(ctx.db, &brought).await;
        return Err(match names.as_slice() {
            [one] => message_with("GangCandidateDodgy1", &[("corpName", Value::text(one))]),
            many => message_with("GangCandidateDodgyN", &[("corpNames", Value::text(many.join(", ")))]),
        });
    }
    let my_name = name_of(ctx.db, me).await;
    let enemy_names = names_of(ctx.db, &gang_enemies).await;
    let (key, args): (&str, Vec<(&str, String)>) = match enemy_names.as_slice() {
        [] => ("AskJoinGang", vec![("name", my_name)]),
        [one] => ("GangMembersDodgy1", vec![("name", my_name), ("corpName", one.clone())]),
        many => ("GangMembersDodgyN", vec![("name", my_name), ("corpNames", many.join(", "))]),
    };

    let mut w = ctx.world.lock();
    let gang_id = match w.gangs.of(me) {
        Some(id) => id,
        None => {
            let id = w.gangs.form(me);
            set_session(&w, me, Some(id), true);
            tell_system(&w, me, Some(id));
            info!("{me} formed gang {id}");
            id
        }
    };
    if let Some(g) = w.gangs.gangs.get_mut(&gang_id) {
        if !g.invited.contains(&invitee) {
            g.invited.push(invitee);
        }
    }
    let dict = Value::Dict(args.into_iter().map(|(k, v)| (Value::bytes(k), Value::text(v))).collect());
    let event = Value::Tuple(vec![Value::Int(me), Value::Int(gang_id), Value::text(key), dict]);
    w.notify(&w.clients_of(invitee), "OnGangInvite", event);
    info!("{me} invited {invitee} to gang {gang_id}");
    Ok(Value::None)
}

/// AcceptInvitation(gangID): out of any other gang, into this one.
fn accept(ctx: &Ctx<'_>, me: i64, gang_id: i64) -> Refused<Value> {
    let mut w = ctx.world.lock();
    let invited = w.gangs.get(gang_id).ok_or_else(|| message("gangNoSuchGang"))?.invited.contains(&me);
    if !invited {
        return Err(message("gangCandidateNotInvited"));
    }
    if let Some(old) = w.gangs.of(me).filter(|g| *g != gang_id) {
        part(&mut w, old, me, false);
    }
    let before = w.gangs.get(gang_id).map(|g| g.members.clone()).unwrap_or_default();
    w.gangs.join(gang_id, me);
    tell(&w, &before, "OnGangJoin", vec![Value::Int(me), Value::text(ACTIVE)]);
    set_session(&w, me, Some(gang_id), false);
    tell_system(&w, me, Some(gang_id));
    info!("{me} joined gang {gang_id}");
    Ok(Value::None)
}

fn leave(w: &mut WorldState, me: i64) -> Refused<()> {
    let gang_id = w.gangs.of(me).ok_or_else(|| message("gangNotMemberOfAnyGang"))?;
    part(w, gang_id, me, true);
    info!("{me} left gang {gang_id}");
    Ok(())
}

/// KickMember(charID): the leader puts a member out; on oneself it is leaving.
fn kick(w: &mut WorldState, me: i64, target: i64) -> Refused<Value> {
    if target == me {
        return leave(w, me).map(|_| Value::None);
    }
    let gang_id = led_by(w, me)?;
    if !w.gangs.gangs[&gang_id].members.contains(&target) {
        return Err(message("gangNotAMember"));
    }
    part(w, gang_id, target, true);
    info!("{me} kicked {target} out of gang {gang_id}");
    Ok(Value::None)
}

fn disband(w: &mut WorldState, me: i64) -> Refused<Value> {
    let gang_id = led_by(w, me)?;
    let members = w.gangs.gangs.remove(&gang_id).map(|g| g.members).unwrap_or_default();
    for m in &members {
        set_session(w, *m, None, false);
        tell_system(w, *m, None);
    }
    info!("{me} disbanded gang {gang_id} ({} member(s))", members.len());
    Ok(Value::None)
}

fn make_leader(w: &mut WorldState, me: i64, target: i64) -> Refused<Value> {
    let gang_id = led_by(w, me)?;
    if !w.gangs.gangs[&gang_id].members.contains(&target) {
        return Err(message("gangNotAMember"));
    }
    set_leader(w, gang_id, Some(target));
    info!("{me} made {target} the leader of gang {gang_id}");
    Ok(Value::None)
}

fn resign(w: &mut WorldState, me: i64) -> Refused<Value> {
    let gang_id = led_by(w, me)?;
    set_leader(w, gang_id, None);
    info!("{me} resigned the leadership of gang {gang_id}");
    Ok(Value::None)
}

/// ClaimLeadership(): a member takes the lead of a gang that has none.
fn claim(w: &mut WorldState, me: i64) -> Refused<Value> {
    let gang_id = w.gangs.of(me).ok_or_else(|| message("gangNotMemberOfAnyGang"))?;
    if w.gangs.gangs[&gang_id].sitting_leader().is_some() {
        return Err(message("gangAlreadyLeader"));
    }
    set_leader(w, gang_id, Some(me));
    info!("{me} claimed the leadership of gang {gang_id}");
    Ok(Value::None)
}

/// A member logged off: they leave their gang.
pub fn logged_off(w: &mut WorldState, char_id: i64) {
    if let Some(gang_id) = w.gangs.of(char_id) {
        part(w, gang_id, char_id, false);
        info!("{char_id} logged off and left gang {gang_id}");
    }
}

/// `char_id` goes from the gang; the rest hear it, and the session follows
/// if they are still here to have one.
fn part(w: &mut WorldState, gang_id: i64, char_id: i64, own_session: bool) {
    let Some(d) = w.gangs.depart(gang_id, char_id) else { return };
    tell(w, &d.rest, "OnGangLeave", vec![Value::Int(char_id)]);
    if d.was_leader {
        tell(w, &d.rest, "OnGangLeader", vec![Value::None]);
    }
    if own_session {
        set_session(w, char_id, None, false);
    }
    tell_system(w, char_id, None);
}

/// The gang the caller leads.
fn led_by(w: &WorldState, me: i64) -> Refused<i64> {
    let gang_id = w.gangs.of(me).ok_or_else(|| message("gangNotMemberOfAnyGang"))?;
    if w.gangs.gangs[&gang_id].sitting_leader() != Some(me) {
        return Err(message("gangMustBeLeader"));
    }
    Ok(gang_id)
}

/// A new leader (or none): their sessions, and everyone told.
fn set_leader(w: &mut WorldState, gang_id: i64, leader: Option<i64>) {
    let Some(g) = w.gangs.gangs.get_mut(&gang_id) else { return };
    let old = g.sitting_leader();
    g.leader = leader;
    let members = g.members.clone();
    if let Some(old) = old.filter(|o| Some(*o) != leader) {
        set_session(w, old, Some(gang_id), false);
    }
    if let Some(new) = leader {
        set_session(w, new, Some(gang_id), true);
    }
    tell(w, &members, "OnGangLeader", vec![Value::int_or_none(leader)]);
}

fn set_session(w: &WorldState, char_id: i64, gang_id: Option<i64>, leader: bool) {
    for client in w.clients_of(char_id) {
        w.push_move(client, Move::Gang { gang_id, leader });
    }
}

fn tell(w: &WorldState, members: &[i64], event: &'static str, args: Vec<Value>) {
    let clients: Vec<i64> = members.iter().flat_map(|m| w.clients_of(*m)).collect();
    w.notify(&clients, event, Value::Tuple(args));
}

/// OnCharGangChange(charID, gangID) to the pilots in space where `char_id` is.
fn tell_system(w: &WorldState, char_id: i64, gang_id: Option<i64>) {
    let Some(p) = w.presence_of(char_id).filter(|p| !p.docked) else { return };
    let event = Value::Tuple(vec![Value::Int(char_id), Value::int_or_none(gang_id)]);
    w.notify(&w.in_space(p.solar_system_id), "OnCharGangChange", event);
}

/// A character's side in a war: its corporation's alliance, or the corporation.
async fn side_of(db: &impl GenericClient, char_id: i64) -> i64 {
    let sql = r#"SELECT COALESCE(NULLIF(co."allianceID", 0), c."corporationID")
                   FROM "character_" c JOIN "corporation" co ON co."corporationID" = c."corporationID"
                  WHERE c."characterID" = $1"#;
    db.query_opt(sql, &[&(char_id as i32)]).await.ok().flatten().and_then(|r| int(&r, 0)).unwrap_or(0)
}

async fn name_of(db: &impl GenericClient, char_id: i64) -> String {
    let sql = r#"SELECT "characterName" FROM "character_" WHERE "characterID" = $1"#;
    db.query_opt(sql, &[&(char_id as i32)]).await.ok().flatten().and_then(|r| text(&r, 0)).unwrap_or_default()
}

/// Corporation and alliance names, in the order given.
async fn names_of(db: &impl GenericClient, owners: &[i64]) -> Vec<String> {
    let mut names = Vec::new();
    for o in owners {
        let sql = r#"SELECT COALESCE((SELECT "corporationName" FROM "corporation" WHERE "corporationID" = $1),
                                     (SELECT "allianceName" FROM "alliances" WHERE "allianceID" = $1), '')"#;
        if let Some(n) = db.query_opt(sql, &[&(*o as i32)]).await.ok().flatten().and_then(|r| text(&r, 0)) {
            names.push(n);
        }
    }
    names
}

fn dedup(mut v: Vec<i64>) -> Vec<i64> {
    v.sort_unstable();
    v.dedup();
    v
}

fn message_with(key: &str, args: &[(&str, Value)]) -> Outcome {
    Outcome::UserError { msg: key.into(), args: args.iter().map(|(k, v)| ((*k).into(), v.clone())).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gang_forms_around_its_leader_and_takes_only_the_invited() {
        let mut gangs = Gangs::default();
        let id = gangs.form(1);
        assert_eq!(gangs.of(1), Some(id));
        assert!(!gangs.join(id, 2));
        gangs.gangs.get_mut(&id).unwrap().invited.push(2);
        assert!(gangs.join(id, 2));
        assert_eq!(gangs.get(id).unwrap().members, vec![1, 2]);
        assert!(gangs.get(id).unwrap().invited.is_empty());
        assert_eq!(gangs.memberships([1, 2, 3]), vec![(1, id), (2, id)]);
    }

    #[test]
    fn the_leader_leaving_leaves_the_gang_leaderless_and_the_last_ends_it() {
        let mut gangs = Gangs::default();
        let id = gangs.form(1);
        gangs.gangs.get_mut(&id).unwrap().invited.push(2);
        gangs.join(id, 2);
        assert_eq!(gangs.depart(id, 1), Some(Departure { rest: vec![2], was_leader: true }));
        assert_eq!(gangs.get(id).unwrap().sitting_leader(), None);
        assert_eq!(gangs.depart(id, 3), None);
        assert_eq!(gangs.depart(id, 2), Some(Departure { rest: vec![], was_leader: false }));
        assert!(gangs.get(id).is_none());
    }
}
