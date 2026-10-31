//! Mission sites: a beacon where an agent's encounter waits, and the rats
//! sent to it. A site opens when its pilot first heads there and lives in
//! the system's park; when the last of its rats dies the mission's
//! objective is done (`Job::SiteCleared`).

use eve_marshal::Value;

use crate::ballpark::{Park, Vec3};
use crate::destiny::{self, Ball};
use crate::svc::space::{self, entry};

/// Celestial Agent Site Beacon.
pub const TYPE_BEACON: i64 = 25354;
const BEACON_RADIUS: f64 = 1_000.0;
const OWNER_SYSTEM: i64 = 1;

#[derive(Debug, Clone)]
pub struct Site {
    pub id: i64,
    pub mission_id: i64,
    pub char_id: i64,
    pub name: String,
    pub pos: Vec3,
    /// The rats still alive.
    pub rats: Vec<i64>,
}

impl Site {
    pub fn ball(&self) -> Ball {
        Ball::celestial(self.id, BEACON_RADIUS, self.pos.tuple(), &self.name, false)
    }

    pub fn slim(&self) -> Value {
        space::slim(vec![
            ("itemID", Value::Int(self.id)),
            ("typeID", Value::Int(TYPE_BEACON)),
            ("ownerID", Value::Int(OWNER_SYSTEM)),
            ("name", Value::text(&self.name)),
        ])
    }

    pub fn radius(&self) -> f64 {
        BEACON_RADIUS
    }
}

/// Opens a site with rats of a strength for `security` around it; returns
/// the destiny entries that show it all.
pub fn open(park: &mut Park, mut site: Site, security: f64, now: i64) -> Vec<Value> {
    let blob = destiny::encode(destiny::stamp(park.tick), &[site.ball()]);
    let chunk = Value::Tuple(vec![Value::Bytes(blob), Value::List(vec![site.slim()]), Value::Dict(Vec::new())]);
    let mut entries = vec![entry(park.tick, "AddBalls", vec![chunk])];
    site.rats = crate::npc::spawn_group(park, site.id, site.pos, security, now);
    for id in &site.rats {
        entries.extend(space::add_ball_entries(park, *id));
    }
    tracing::info!("mission {} site {} opened with {} rat(s)", site.mission_id, site.id, site.rats.len());
    park.sites.insert(site.id, site);
    entries
}

/// Closes a mission's site, and sends off any rats left there.
pub fn close(park: &mut Park, mission_id: i64) -> Vec<Value> {
    let Some(id) = park.sites.values().find(|s| s.mission_id == mission_id).map(|s| s.id) else { return Vec::new() };
    let site = park.sites.remove(&id).expect("the site was just found");
    let mut gone = vec![Value::Int(id)];
    for rat in site.rats {
        if park.ships.remove(&rat).is_some() {
            gone.push(Value::Int(rat));
        }
    }
    vec![entry(park.tick, "RemoveBalls", vec![Value::List(gone)])]
}

/// A rat died: if it was a site's, and the last, the site's mission is done.
pub fn rat_killed(park: &mut Park, site_id: i64, rat: i64) -> Option<(i64, i64)> {
    let site = park.sites.get_mut(&site_id)?;
    site.rats.retain(|r| *r != rat);
    site.rats.is_empty().then_some((site.mission_id, site.char_id))
}
