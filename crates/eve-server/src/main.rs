mod ballpark;
mod bulkdata;
mod combat;
mod concord;
mod config;
mod conn;
mod db;
mod destiny;
mod drones;
mod dogma;
mod exceptions;
mod fight;
mod guard;
mod heartbeat;
mod items;
mod logging;
mod losses;
mod loot;
mod npc;
mod pos;
mod services;
mod session;
mod svc;
mod world;

use std::sync::Arc;
use std::time::Instant;

use eve_marshal::StringTable;
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};

use config::Config;
use conn::{Conn, Server};

/// Bulk objects this close to the 1 MB frame cap are worth a warning.
const BULK_WARN_BYTES: usize = 900_000;

#[tokio::main]
async fn main() {
    if let Err(e) = logging::init() {
        eprintln!("{e}");
        std::process::exit(2);
    }
    if let Err(e) = run().await {
        error!("{}", logging::chain(&*e));
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;

    let table = std::fs::read(&config.strings)
        .map(|raw| StringTable::parse(&raw))
        .map_err(|e| {
            format!("cannot read the shared string table at {}: {e}", config.strings.display())
        })?;
    info!("string table: {} entries from {}", table.len(), config.strings.display());

    let client = db::connect(&config.database).await.map_err(|e| {
        format!("cannot reach PostgreSQL at {}: {}", config.database_redacted(), logging::chain(&e))
    })?;
    let accounts: i64 = client
        .query_one(r#"SELECT count(*) FROM "account""#, &[])
        .await
        .map(|r| r.get(0))
        .unwrap_or(0);
    info!("database: {} ({accounts} accounts)", config.database_redacted());

    let started = Instant::now();
    let bulk =
        bulkdata::BulkData::load(&client, &table, config.node_id, bulkdata::filetime_now()).await?;
    report_bulk(&bulk, started);

    let dogma = dogma::Catalog::load(&client).await.map_err(|e| format!("loading dogma: {}", logging::chain(&e)))?;

    let services = svc::registry();
    info!("{} services registered", services.len());

    let server = Arc::new(Server {
        table,
        db: client,
        db_url: config.database.clone(),
        node_id: config.node_id,
        bulk,
        dogma,
        services,
        world: world::World::default(),
        dump_dir: config.dump_dir.clone(),
        alliance_join_delay_secs: config.alliance_join_delay_secs,
        sov_claim_delay_secs: config.sov_claim_delay_secs,
    });
    pos::set_speedup(config.starbase_speedup);
    tokio::spawn(heartbeat::run(Arc::clone(&server)));
    let listener = TcpListener::bind(&config.bind).await?;
    info!("listening on {} as node {}", config.bind, config.node_id);

    let mut seq = 0i64;
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!("accept failed: {err}", err = crate::logging::chain(&e));
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        seq += 1;
        tokio::spawn(Conn::new(stream, peer, Arc::clone(&server), seq).run());
    }
}

fn report_bulk(bulk: &bulkdata::BulkData, started: Instant) {
    let mut bytes = 0usize;
    let mut empty = Vec::new();
    for spec in bulkdata::SPECS {
        let e = &bulk.entries[spec.key];
        bytes += e.pickle.len();
        if e.rows == 0 {
            empty.push(spec.key);
        }
        if e.pickle.len() > BULK_WARN_BYTES {
            warn!("bulk object {} is {} bytes, near the 1 MB frame cap", spec.key, e.pickle.len());
        }
    }
    info!(
        "bulk data: {} objects, {} rows, {:.1} MB on the wire, built in {:.1}s",
        bulk.entries.len(),
        bulk.entries.values().map(|e| e.rows).sum::<usize>(),
        bytes as f64 / 1e6,
        started.elapsed().as_secs_f64()
    );
    if !empty.is_empty() {
        debug!("{} bulk object(s) served with no rows: {}", empty.len(), empty.join(", "));
    }
}
