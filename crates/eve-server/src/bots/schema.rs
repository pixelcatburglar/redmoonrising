//! The tables the bot simulation keeps beyond the characters themselves,
//! made on start where an older database lacks them (the dumps in `db/`
//! hold them too, for a fresh one).

use tokio_postgres::Client;

const STATEMENTS: &[&str] = &[
    // The ship class a bot has worked its way up to (`ships`).
    r#"ALTER TABLE "bots" ADD COLUMN IF NOT EXISTS "tier" smallint NOT NULL DEFAULT 0"#,
    // What the bots hold in each station, between them (`economy`).
    r#"CREATE TABLE IF NOT EXISTS "botStock" (
           "stationID" integer NOT NULL,
           "typeID" integer NOT NULL,
           "quantity" bigint NOT NULL DEFAULT 0,
           PRIMARY KEY ("stationID", "typeID"))"#,
    r#"COMMENT ON TABLE "botStock" IS 'MADE: goods the bot simulation holds in each station: ore mined, minerals refined, hulls built (bots::economy).'"#,
    // How each bot side feels about each player (`relations`).
    r#"CREATE TABLE IF NOT EXISTS "botRelations" (
           "sideID" integer NOT NULL,
           "charID" integer NOT NULL,
           "heat" real NOT NULL DEFAULT 0,
           "favor" real NOT NULL DEFAULT 0,
           "flags" integer NOT NULL DEFAULT 0,
           "updatedAt" bigint NOT NULL DEFAULT 0,
           PRIMARY KEY ("sideID", "charID"))"#,
    r#"COMMENT ON TABLE "botRelations" IS 'MADE: what each bot alliance (or corporation outside one) holds against or for each player: heat from losses, favor from help, and what it has done about it (bots::relations).'"#,
    // How busy each region is, against the usual (`control`).
    r#"CREATE TABLE IF NOT EXISTS "botRegionDensity" (
           "regionID" integer PRIMARY KEY,
           "factor" real NOT NULL DEFAULT 1)"#,
    r#"COMMENT ON TABLE "botRegionDensity" IS 'MADE: how much more or less the bots living in a region play than usual, set from the dashboard or /bots density (bots::control).'"#,
    // Kill reports share one sequence: losses.rs and the bots both take from it.
    r#"SELECT setval(pg_get_serial_sequence('"killLog"', 'killID'), GREATEST((SELECT COALESCE(MAX("killID"), 0) FROM "killLog"), 1))"#,
];

pub async fn ensure(db: &Client) -> Result<(), tokio_postgres::Error> {
    for sql in STATEMENTS {
        db.execute(*sql, &[]).await?;
    }
    Ok(())
}
