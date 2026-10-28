use std::path::PathBuf;

/// Server settings, read from `RMR_*` environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    /// `RMR_BIND`: the address the client connects to.
    pub bind: String,
    /// `RMR_DATABASE`: a PostgreSQL connection URL.
    pub database: String,
    /// `RMR_NODE_ID`: this node's machoNet ID, also the low digits of every clientID.
    pub node_id: i64,
    /// `RMR_STRINGS`: the client's shared marshal string table.
    pub strings: PathBuf,
    /// `RMR_DUMP_DIR`: where undecodable frames are saved.
    pub dump_dir: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let node_id = var("RMR_NODE_ID", "1");
        let node_id: i64 =
            node_id.parse().map_err(|_| format!("RMR_NODE_ID is not a number: {node_id:?}"))?;
        if !(0..10_000_000).contains(&node_id) {
            return Err(format!("RMR_NODE_ID must be in 0..10000000, got {node_id}"));
        }
        Ok(Config {
            bind: var("RMR_BIND", "0.0.0.0:26000"),
            database: var("RMR_DATABASE", "postgres://rmr:rmr@127.0.0.1:55432/rmr"),
            node_id,
            strings: var(
                "RMR_STRINGS",
                concat!(env!("CARGO_MANIFEST_DIR"), "/../eve-marshal/tests/data/strings.txt"),
            )
            .into(),
            dump_dir: var("RMR_DUMP_DIR", ".").into(),
        })
    }

    /// The database URL with any password masked, for logging.
    pub fn database_redacted(&self) -> String {
        let url = &self.database;
        let (Some(scheme_end), Some(at)) = (url.find("://"), url.rfind('@')) else {
            return url.clone();
        };
        let creds = &url[scheme_end + 3..at];
        match creds.split_once(':') {
            Some((user, _)) => format!("{}{user}:***{}", &url[..scheme_end + 3], &url[at..]),
            None => url.clone(),
        }
    }
}

fn var(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_db(url: &str) -> Config {
        Config {
            bind: String::new(),
            database: url.to_string(),
            node_id: 1,
            strings: PathBuf::new(),
            dump_dir: PathBuf::new(),
        }
    }

    #[test]
    fn the_password_is_masked_in_the_logged_url() {
        assert_eq!(
            with_db("postgres://rmr:secret@127.0.0.1:5432/rmr").database_redacted(),
            "postgres://rmr:***@127.0.0.1:5432/rmr"
        );
        assert_eq!(
            with_db("postgres://127.0.0.1/rmr").database_redacted(),
            "postgres://127.0.0.1/rmr"
        );
    }
}
