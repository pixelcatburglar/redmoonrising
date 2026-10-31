//! Log output: coloured, levelled lines on stdout, optionally mirrored to a file.
//!
//! - The level comes from the config file (`log.level`), or `RMR_LOG` (or
//!   `RUST_LOG`): `off`, `error`, `warn`, `info`, `debug` or `trace`, or a
//!   filter of its own such as `info,eve_server::svc=trace`. `debug`, the
//!   default, shows every service call and hides library chatter.
//! - `log.file` (or `RMR_LOG_FILE`) also appends plain, uncoloured lines to that file.
//! - `NO_COLOR` turns colour off; it is also off when stdout is not a terminal.
//!
//! Every line from a connection carries its `conn{..}` span (clientID, peer, and the
//! user and character once known), and every line logged while serving a call
//! carries `call{svc=.. method=..}` as well.

use std::fs::OpenOptions;
use std::io::IsTerminal;
use std::sync::Mutex;

use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::format::{DefaultFields, Writer};
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

const TIME_FORMAT: &str = "%H:%M:%S%.3f";

/// The filter a level stands for. Below `info`, only the server's own lines
/// are let through: the libraries' are noise.
pub fn filter_for(level: &str) -> String {
    match level.trim().to_ascii_lowercase().as_str() {
        "off" | "none" => "off".into(),
        "error" => "error".into(),
        "warn" | "warning" => "warn".into(),
        "info" => "info".into(),
        "debug" => "info,eve_server=debug".into(),
        "trace" => "info,eve_server=trace".into(),
        _ => level.trim().to_string(),
    }
}

pub fn init(level: &str, file: Option<&std::path::Path>) -> Result<(), String> {
    let filter = filter_for(level);
    let filter = EnvFilter::try_new(&filter).map_err(|e| format!("bad log level {level:?}: {e}"))?;

    let console = fmt::layer()
        .with_timer(ChronoLocal::new(TIME_FORMAT.into()))
        .with_target(false)
        .with_ansi(use_colour());

    let file = match file {
        Some(path) => {
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("cannot open the log file {}: {e}", path.display()))?;
            Some(
                fmt::layer()
                    .with_timer(ChronoLocal::new(format!("%Y-%m-%d {TIME_FORMAT}")))
                    .with_target(true)
                    .with_ansi(false)
                    .fmt_fields(FileFields::default())
                    .with_writer(Mutex::new(f)),
            )
        }
        None => None,
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(console)
        .with(file)
        .try_init()
        .map_err(|e| e.to_string())
}

/// An error followed by its causes. Needed for database errors: a
/// `tokio_postgres::Error` displays only as "db error" and keeps what actually
/// went wrong (`relation "x" does not exist`) in its source.
pub fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        out.push_str(": ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
}

/// The default field formatter under another type. Span fields are cached per
/// formatter type, so if both layers used `DefaultFields` they would share one
/// cache: recorded fields (`user`, `char`) would appear twice, and the file would
/// get the console's colour codes.
#[derive(Default)]
struct FileFields(DefaultFields);

impl<'w> FormatFields<'w> for FileFields {
    fn format_fields<R: RecordFields>(&self, writer: Writer<'w>, fields: R) -> std::fmt::Result {
        self.0.format_fields(writer, fields)
    }
}

fn use_colour() -> bool {
    if std::env::var_os("NO_COLOR").is_some() || !std::io::stdout().is_terminal() {
        return false;
    }
    // Windows consoles only interpret ANSI escapes once asked to.
    #[cfg(windows)]
    if nu_ansi_term::enable_ansi_support().is_err() {
        return false;
    }
    true
}
