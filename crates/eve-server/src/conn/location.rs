//! Calls that move the session: entering the game, undocking, docking and
//! jumping. Each also moves the ship into or out of its system's ballpark, so
//! the other players there see it arrive and leave.

use std::time::Duration;

use eve_marshal::Value;
use eve_proto::MachoPacket;
use tracing::{error, info, warn};

use super::{Conn, Result};
use crate::ballpark::{Command, Ship, Vec3};
use crate::destiny;
use crate::exceptions::{custom_error, user_error};
use crate::svc::inventory::FLAG_HANGAR;
use crate::svc::{bound, space};

/// const.maxStargateJumpingDistance.
const JUMP_RANGE: f64 = 2_500.0;
/// The client runs its own simulation; allow for the two drifting apart.
const RANGE_SLACK: f64 = 1.25;
/// Close enough to a station's surface to be towed in.
const DOCK_RANGE: f64 = 2_500.0;
/// The longest a docking approach may take before the request is refused.
const DOCK_APPROACH_LIMIT: i64 = 600;

impl Conn {
    pub(super) async fn select_character(&mut self, req: &MachoPacket, char_id: i64) -> Result {
        let before = self.session.clone();

        let sql = r#"SELECT c."characterID", c."corporationID", c."stationID", c."solarSystemID",
                            c."constellationID", c."regionID",
                            co."allianceID", co."stationID" AS "hqID", c."shipID",
                            c."characterName", c."typeID"
                       FROM "character_" c
                       LEFT JOIN "corporation" co ON co."corporationID" = c."corporationID"
                      WHERE c."characterID" = $1 AND c."accountID" = $2"#;
        let user_id = self.session.user_id.unwrap_or(0);
        let rows = match self.server.db.query(sql, &[&(char_id as i32), &(user_id as i32)]).await {
            Ok(r) => r,
            Err(e) => {
                error!("loading character {char_id}: {}", crate::logging::chain(&e));
                let err = custom_error("That character could not be loaded.");
                return self.send_error(req, err).await;
            }
        };
        let Some(row) = rows.first() else {
            warn!("character {char_id} is not on account {user_id}");
            let err = custom_error("That character is not on this account.");
            return self.send_error(req, err).await;
        };

        let opt = |i: usize| crate::svc::rows::int(row, i);
        let opt_id = |i: usize| opt(i).filter(|v| *v != 0);
        self.session.char_id = Some(char_id);
        self.session.char_name = crate::svc::rows::text(row, 9).unwrap_or_default();
        self.session.char_type_id = opt(10).unwrap_or(0);
        self.session.corp_id = opt(1);
        self.session.station_id = opt(2);
        self.session.solar_system_id = opt(3);
        self.session.constellation_id = opt(4);
        self.session.region_id = opt(5);
        self.session.alliance_id = opt_id(6);
        self.session.hq_id = opt_id(7);
        self.session.base_id = self.session.hq_id;
        self.session.solar_system_id2 = opt(3);
        if self.session.station_id.is_some() {
            self.session.solar_system_id = None;
        }
        self.session.ship_id = match self.session.station_id {
            Some(station) => self.resolve_active_ship(char_id, station).await,
            None => opt(8),
        };
        self.span.record("char", char_id);

        self.send_session_change_from(&before).await?;
        info!(
            corp = ?self.session.corp_id,
            station = ?self.session.station_id,
            system = ?self.session.solar_system_id2,
            ship = ?self.session.ship_id,
            "entered the game as {:?} ({char_id})",
            self.session.char_name
        );
        self.reply(req, &Value::None).await
    }

    pub(super) async fn undock(&mut self, req: &MachoPacket, bind_params: Option<Vec<i64>>) -> Result {
        let (Some(station_id), Some(system_id)) =
            (self.session.station_id, self.session.solar_system_id2)
        else {
            warn!("undock while not docked");
            return self.send_error(req, custom_error("You are not docked.")).await;
        };
        let Some(ship_id) = self.session.ship_id else {
            warn!("undock with no active ship");
            let err = custom_error("You have no active ship to undock in.");
            return self.send_error(req, err).await;
        };

        let (loaded, exit) = {
            let ctx = self.ctx();
            space::ensure_park(&ctx, system_id).await;
            (space::load_ship(&ctx, ship_id).await, space::undock_point(&ctx, station_id).await)
        };
        let (Some((stats, pilot)), Some((pos, heading))) = (loaded, exit) else {
            error!("cannot undock ship {ship_id} from {station_id}: ship or station data missing");
            return self.send_error(req, custom_error("Your ship could not be undocked.")).await;
        };
        // Out of the bay at full speed, coasting to a stop clear of the station.
        let push = heading * stats.max_velocity;
        let ship = Ship::new(ship_id, self.client_id, stats, pilot, pos, push);

        let before = self.session.clone();
        self.session.station_id = None;
        self.session.solar_system_id = Some(system_id);
        self.session.solar_system_id2 = Some(system_id);
        self.send_session_change_from(&before).await?;
        space::add_ship(&mut self.server.world.lock(), system_id, ship);
        info!("undocked from station {station_id} into system {system_id} in ship {ship_id}");

        let retval = match bind_params {
            Some(params) => bound::bind_result(self.server.node_id, "ship", &params, Value::None),
            None => Value::None,
        };
        self.reply(req, &retval).await
    }

    /// ship.Board(shipID) while docked: make another of the character's
    /// assembled ships in this station the active one.
    pub(super) async fn board(&mut self, req: &MachoPacket, ship_id: i64, bind_params: Option<Vec<i64>>) -> Result {
        let (Some(char_id), Some(station_id)) = (self.session.char_id, self.session.station_id) else {
            return self.send_error(req, custom_error("Ships can only be boarded in a station.")).await;
        };
        const SQL: &str = r#"
            UPDATE "character_" SET "shipID" = $2
             WHERE "characterID" = $1
               AND EXISTS (SELECT 1 FROM entity e
                             JOIN "invTypes" t ON t."typeID" = e."typeID"
                             JOIN "invGroups" g ON g."groupID" = t."groupID"
                            WHERE e."itemID" = $2 AND e."ownerID" = $1 AND e."locationID" = $3
                              AND e."singleton" = 1 AND g."categoryID" = 6)
            RETURNING "shipID""#;
        let params: [&(dyn tokio_postgres::types::ToSql + Sync); 3] =
            [&(char_id as i32), &(ship_id as i32), &(station_id as i32)];
        match self.server.db.query_opt(SQL, &params).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                warn!("board {ship_id}: not an assembled ship of {char_id} in {station_id}");
                return self.send_error(req, custom_error("You cannot board that ship.")).await;
            }
            Err(e) => {
                error!("boarding {ship_id}: {}", crate::logging::chain(&e));
                return self.send_error(req, custom_error("The ship could not be boarded.")).await;
            }
        }
        let before = self.session.clone();
        self.session.ship_id = Some(ship_id);
        self.send_session_change_from(&before).await?;
        info!("boarded ship {ship_id}");
        let retval = match bind_params {
            Some(params) => bound::bind_result(self.server.node_id, "ship", &params, Value::None),
            None => Value::None,
        };
        self.reply(req, &retval).await
    }

    pub(super) async fn dock(&mut self, req: &MachoPacket, station_id: i64, towed: bool) -> Result {
        let denied = |reason: &str| user_error("DockingRequestDenied", &[("reason", Value::bytes(reason))]);
        let (Some(system_id), Some(ship_id)) = (self.session.solar_system_id, self.session.ship_id) else {
            warn!("dock while not in space");
            return self.send_error(req, denied("You are not in space.")).await;
        };

        // Where is the ship relative to the station? (Decided under the lock,
        // answered after it is released.)
        let gap = {
            let mut world = self.server.world.lock();
            match world.parks.get_mut(&system_id) {
                None => Err("You are not in space."),
                Some(park) => {
                    park.advance(destiny::tick_now());
                    if !space::is_station(park, station_id) {
                        warn!("dock at {station_id}, which is not a station in {system_id}");
                        Err("That station is not in this solar system.")
                    } else {
                        park.surface_distance(ship_id, station_id).ok_or("Your ship is not in space.")
                    }
                }
            }
        };
        let gap = match gap {
            Ok(gap) => gap,
            Err(reason) => return self.send_error(req, denied(reason)).await,
        };

        // Too far: set course for the station and wait for the ship to get there.
        if gap > DOCK_RANGE && !towed {
            let eta = {
                let mut world = self.server.world.lock();
                let park = world.parks.get_mut(&system_id).expect("checked above");
                park.command(ship_id, Command::Follow { target: station_id, range: 0.0 });
                let eta = park.eta_within(ship_id, station_id, DOCK_RANGE, DOCK_APPROACH_LIMIT);
                let course = space::entry(
                    park.tick,
                    "FollowBall",
                    vec![Value::Int(ship_id), Value::Int(station_id), Value::Double(0.0)],
                );
                space::broadcast(&world, system_id, None, vec![course]);
                let message = Value::Tuple(vec![Value::bytes("DockingApproach"), Value::Dict(Vec::new())]);
                world.notify(&[self.client_id], "OnRemoteMessage", message);
                eta
            };
            let Some(eta) = eta else {
                warn!("dock at {station_id}: {gap:.0} m away, too far to approach");
                return self.send_error(req, denied("The station is too far away.")).await;
            };
            info!("approaching station {station_id} from {gap:.0} m, docking in ~{eta}s");
            tokio::time::sleep(Duration::from_secs(eta as u64 + 1)).await;
        }

        // Towed in: out of space, into the station.
        let (start, end) = {
            let mut world = self.server.world.lock();
            let start = world
                .parks
                .get(&system_id)
                .and_then(|p| p.ships.get(&ship_id))
                .map_or(Vec3::ZERO, |s| s.pos);
            let end = world.parks.get(&system_id).and_then(|p| p.ball(station_id)).map_or(start, |(p, _)| p);
            space::remove_ship(&mut world, system_id, ship_id);
            (start, end)
        };
        let point = |v: Vec3| Value::Tuple(vec![Value::Double(v.x), Value::Double(v.y), Value::Double(v.z)]);
        let accepted = Value::Tuple(vec![point(start), point(end), Value::Int(station_id)]);
        self.server.world.lock().notify(&[self.client_id], "OnDockingAccepted", accepted);

        let before = self.session.clone();
        self.session.station_id = Some(station_id);
        self.session.solar_system_id = None;
        self.session.solar_system_id2 = Some(system_id);
        if let Some(char_id) = self.session.char_id {
            self.session.ship_id = self.store_docked(char_id, ship_id, station_id).await;
        }
        self.send_session_change_from(&before).await?;

        info!(ship = ?self.session.ship_id, "docked at station {station_id}");
        self.reply(req, &Value::None).await
    }

    pub(super) async fn stargate_jump(&mut self, req: &MachoPacket, from_gate: i64, to_gate: i64) -> Result {
        let (Some(system_id), Some(ship_id)) = (self.session.solar_system_id, self.session.ship_id) else {
            warn!("stargate jump while not in space in a ship");
            return self.send_error(req, custom_error("You are not in space.")).await;
        };

        let gap = {
            let mut world = self.server.world.lock();
            world.parks.get_mut(&system_id).and_then(|park| {
                park.advance(destiny::tick_now());
                park.surface_distance(ship_id, from_gate)
            })
        };
        match gap {
            Some(gap) if gap <= JUMP_RANGE * RANGE_SLACK => {}
            Some(gap) => {
                info!("jump refused: {gap:.0} m from gate {from_gate}");
                let maxdist = [("maxdist", Value::Int(JUMP_RANGE as i64))];
                return self.send_error(req, user_error("NotCloseEnoughToJump", &maxdist)).await;
            }
            None => {
                warn!("jump: ship {ship_id} or gate {from_gate} is not in the ballpark of {system_id}");
                return self.send_error(req, custom_error("You are not in space.")).await;
            }
        }

        let jump = {
            let ctx = self.ctx();
            match space::resolve_jump(&ctx, system_id, from_gate, to_gate).await {
                Some(jump) => {
                    space::ensure_park(&ctx, jump.solar_system_id).await;
                    Some(jump)
                }
                None => None,
            }
        };
        let Some(jump) = jump else {
            warn!("gate {from_gate} -> {to_gate} is not a jump from system {system_id}");
            let err = custom_error("That stargate does not lead there.");
            return self.send_error(req, err).await;
        };

        // Leave: everyone here sees the jump flash, then the ship goes.
        let ship = {
            let mut world = self.server.world.lock();
            let tick = world.parks.get(&system_id).map_or(destiny::tick_now(), |p| p.tick);
            let flash = space::special_fx(tick, ship_id, Some(from_gate), "effects.JumpOut");
            space::broadcast(&world, system_id, None, vec![flash]);
            space::remove_ship(&mut world, system_id, ship_id)
        };

        let before = self.session.clone();
        self.session.solar_system_id = Some(jump.solar_system_id);
        self.session.solar_system_id2 = Some(jump.solar_system_id);
        self.session.constellation_id = Some(jump.constellation_id);
        self.session.region_id = Some(jump.region_id);
        self.send_session_change_from(&before).await?;

        // Arrive: out of the destination gate, standing still.
        if let Some(mut ship) = ship {
            ship.pos = jump.arrival;
            ship.vel = Vec3::ZERO;
            ship.command = Command::Stop;
            let mut world = self.server.world.lock();
            space::add_ship(&mut world, jump.solar_system_id, ship);
            let tick = world.parks.get(&jump.solar_system_id).map_or(destiny::tick_now(), |p| p.tick);
            let flash = space::special_fx(tick, ship_id, Some(to_gate), "effects.JumpIn");
            let others: Vec<i64> =
                world.in_space(jump.solar_system_id).into_iter().filter(|c| *c != self.client_id).collect();
            for client in others {
                space::broadcast(&world, jump.solar_system_id, Some(client), vec![flash.clone()]);
            }
        }

        info!(
            "ship {ship_id} jumped from gate {from_gate} in {system_id} to gate {to_gate} in {}",
            jump.solar_system_id
        );
        self.reply(req, &Value::None).await
    }

    /// Record a dock: the ship goes into the station hangar, the pilot into the
    /// station, and the ship stays the active one. Returns the active ship.
    async fn store_docked(&self, char_id: i64, ship_id: i64, station_id: i64) -> Option<i64> {
        const SQL: &str = r#"
            WITH station AS (
                SELECT "stationID","solarSystemID","constellationID","regionID"
                  FROM "staStations" WHERE "stationID" = $3
            ),
            ship AS (
                UPDATE entity SET "locationID" = $3, "flag" = $4
                 WHERE "itemID" = $2 AND "ownerID" = $1
                RETURNING "itemID"
            ),
            pilot AS (
                UPDATE entity SET "locationID" = $3 WHERE "itemID" = $1
            )
            UPDATE "character_" c
               SET "stationID" = s."stationID", "solarSystemID" = s."solarSystemID",
                   "constellationID" = s."constellationID", "regionID" = s."regionID",
                   "shipID" = (SELECT "itemID" FROM ship)
              FROM station s
             WHERE c."characterID" = $1
            RETURNING c."shipID""#;
        let params: [&(dyn tokio_postgres::types::ToSql + Sync); 4] =
            [&(char_id as i32), &(ship_id as i32), &(station_id as i32), &FLAG_HANGAR];
        match self.server.db.query_opt(SQL, &params).await {
            Ok(row) => row.and_then(|r| crate::svc::rows::int(&r, 0)),
            Err(e) => {
                error!("recording {char_id} docked at {station_id}: {}", crate::logging::chain(&e));
                None
            }
        }
    }

    /// Pick the character's ship in `station_id` (preferring the recorded one) and
    /// store it as their active ship.
    async fn resolve_active_ship(&self, char_id: i64, station_id: i64) -> Option<i64> {
        const SQL: &str = r#"
            WITH candidate AS (
                SELECT e."itemID"
                  FROM entity e
                  JOIN "invTypes"  t ON t."typeID"  = e."typeID"
                  JOIN "invGroups" g ON g."groupID" = t."groupID"
                 WHERE e."ownerID"    = $1
                   AND e."locationID" = $2
                   AND g."categoryID" = 6
                   AND e."singleton"  = 1
                 ORDER BY (e."itemID" = (SELECT c."shipID" FROM "character_" c
                                          WHERE c."characterID" = $1)) DESC,
                          e."itemID"
                 LIMIT 1
            )
            UPDATE "character_" SET "shipID" = (SELECT "itemID" FROM candidate)
             WHERE "characterID" = $1
             RETURNING "shipID""#;
        match self.server.db.query_opt(SQL, &[&(char_id as i32), &(station_id as i32)]).await {
            Ok(row) => row.and_then(|r| crate::svc::rows::int(&r, 0)),
            Err(e) => {
                error!("resolving the active ship for {char_id}: {}", crate::logging::chain(&e));
                None
            }
        }
    }
}
