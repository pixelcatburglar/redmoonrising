pub mod agents;
pub mod alert;
pub mod alliance;
pub mod bookmark;
pub mod bound;
pub mod charcreation;
pub mod charsheet;
pub mod chat;
pub mod clones;
pub mod config;
pub mod corp;
pub mod dogma;
pub mod factory;
pub mod fitting;
pub mod gang;
pub mod inflight;
pub mod insurance;
pub mod inventory;
pub mod language;
pub mod login;
pub mod machonet;
pub mod lookup;
pub mod medical;
pub mod map;
pub mod market;
pub mod membership;
pub mod moonmining;
pub mod missions;
pub mod moves;
pub mod notes;
pub mod onlinestatus;
pub mod pvptrade;
pub mod petition;
pub mod ram;
pub mod repair;
pub mod reprocessing;
pub mod rows;
pub mod skills;
pub mod space;
pub mod starbase;
pub mod standing;
pub mod station;
pub mod trade;
pub mod votes;
pub mod tutorial;
pub mod war;
pub mod wallet;

use crate::services::{handler, Registry};

/// Every service the server answers. A name listed more than once is served by
/// several modules, tried in the order given here.
///
/// Calls that change the session (`character::SelectCharacterID`, `ship::Undock`,
/// `beyonce::Dock` / `StargateJump` / movement) never get here: `conn` serves them.
pub fn registry() -> Registry {
    let mut r = Registry::default();
    r.add("objectCaching", handler!(login::object_caching))
        .add("authentication", handler!(login::authentication))
        .add("alert", handler!(alert::alert))
        .add("config", handler!(config::config))
        .add("languageSvc", handler!(language::language_svc))
        .add("machoNet", handler!(machonet::macho_net))
        // character
        .add("character", handler!(login::character))
        .add("character", handler!(charcreation::character))
        .add("character", handler!(charsheet::character))
        .add("character", handler!(notes::character))
        .add("charmgr", handler!(charsheet::charmgr))
        .add_bound("skillMgr", handler!(skills::skill_mgr))
        .add_bound("jumpCloneSvc", handler!(clones::jump_clone_svc))
        .add("standing2", handler!(standing::standing))
        // station and map
        .add("station", handler!(station::station))
        .add("stationSvc", handler!(station::station_svc))
        .add("map", handler!(map::map))
        .add("map", handler!(station::map))
        .add("tutorialSvc", handler!(tutorial::tutorial_svc))
        // items and ships
        .add_bound("invbroker", handler!(inventory::invbroker))
        .add("invcontainer", handler!(inventory::invcontainer))
        .add_bound("dogmaIM", handler!(starbase::dogma_im))
        .add_bound("dogmaIM", handler!(inflight::dogma_im))
        .add_bound("dogmaIM", handler!(dogma::dogma_im))
        .add("factory", handler!(factory::factory))
        .add("ramProxy", handler!(ram::ram_proxy))
        .add_bound("reprocessingSvc", handler!(reprocessing::reprocessing_svc))
        .add_bound("repairSvc", handler!(repair::repair_svc))
        .add_bound("ship", handler!(space::ship))
        .add_bound("insuranceSvc", handler!(insurance::insurance_svc))
        .add_bound("beyonce", handler!(space::beyonce))
        .add_bound("posMgr", handler!(starbase::pos_mgr))
        .add_bound("entity", handler!(crate::drones::entity))
        .add_bound("aggressionMgr", handler!(crate::concord::aggression_mgr))
        // money and market
        .add("account", handler!(wallet::account))
        .add("billMgr", handler!(wallet::bill_mgr))
        .add("marketProxy", handler!(market::market_proxy))
        .add("marketProxy", handler!(wallet::market_proxy))
        .add("trademgr", handler!(pvptrade::trademgr))
        .add_bound("tradeSession", handler!(pvptrade::session))
        // corporations and alliances
        .add("corpmgr", handler!(corp::corpmgr))
        .add("corporationSvc", handler!(corp::corporation_svc))
        .add_bound("corpRegistry", handler!(membership::corp_registry))
        .add_bound("corpRegistry", handler!(alliance::corp_registry))
        .add_bound("corpRegistry", handler!(war::corp_registry))
        .add_bound("corpRegistry", handler!(votes::corp_registry))
        .add_bound("corpRegistry", handler!(corp::registry))
        .add_bound("corpStationMgr", handler!(corp::station_mgr))
        .add_bound("corpMembers", handler!(corp::members_provider))
        .add_bound("allianceRegistry", handler!(alliance::alliance_registry))
        .add_bound("warRegistry", handler!(war::war_registry))
        // social
        .add("LSC", handler!(chat::lsc))
        .add("onlineStatus", handler!(onlinestatus::online_status))
        .add("bookmark", handler!(bookmark::bookmark))
        .add("petitioner", handler!(petition::petitioner))
        .add("lookupSvc", handler!(lookup::lookup_svc))
        .add("gangsta", handler!(gang::gangsta))
        // agents and missions
        .add_bound("agentMgr", handler!(agents::agent_mgr))
        .add("missionMgr", handler!(missions::mission_mgr));
    r
}
