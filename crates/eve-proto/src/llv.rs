use crate::error::{ProtoError, Result};
use eve_marshal::Value;

pub const LLV_MAGIC: i64 = 170472;

pub const MACHO_VERSION: i64 = 139;

pub const BOOT_VERSION: f64 = 2.1;

pub const BOOT_BUILD: i64 = 3913;

pub const BOOT_CODENAME: &[u8] = b"Blood";

pub const MAX_CLIENT_LLV_BYTES: usize = 1000;

pub const LLV_TIMEOUT_SECS: u64 = 120;

#[derive(Clone, Debug, PartialEq)]
pub struct VersionChallenge {
    pub magic: i64,
    pub macho_version: i64,
    pub user_count: i64,
    pub boot_version: f64,
    pub boot_build: i64,
    pub codename: Vec<u8>,
}

impl VersionChallenge {
    pub fn server(user_count: i64) -> Self {
        VersionChallenge {
            magic: LLV_MAGIC,
            macho_version: MACHO_VERSION,
            user_count,
            boot_version: BOOT_VERSION,
            boot_build: BOOT_BUILD,
            codename: BOOT_CODENAME.to_vec(),
        }
    }

    pub fn client() -> Self {
        Self::server(0)
    }

    pub fn to_value(&self) -> Value {
        Value::Tuple(vec![
            Value::Int(self.magic),
            Value::Int(self.macho_version),
            Value::Int(self.user_count),
            Value::Double(self.boot_version),
            Value::Int(self.boot_build),
            Value::Bytes(self.codename.clone()),
        ])
    }

    pub fn from_value(v: &Value) -> Result<Self> {
        let items = v.seq().ok_or(ProtoError::VersionMismatch("not a tuple"))?;
        if items.len() != 6 {
            return Err(ProtoError::VersionMismatch("challenge is not a 6-tuple"));
        }
        let int = |i: usize, what: &'static str| {
            items[i].as_i64().ok_or(ProtoError::VersionMismatch(what))
        };
        Ok(VersionChallenge {
            magic: int(0, "magic is not an integer")?,
            macho_version: int(1, "version is not an integer")?,
            user_count: int(2, "user count is not an integer")?,
            boot_version: match &items[3] {
                Value::Double(d) => *d,
                other => other.as_i64().map(|i| i as f64).ok_or(
                    ProtoError::VersionMismatch("boot version is not a number"),
                )?,
            },
            boot_build: int(4, "build is not an integer")?,
            codename: items[5].as_bytes().unwrap_or_default().to_vec(),
        })
    }

    pub fn validate(&self) -> Result<()> {
        if self.magic != LLV_MAGIC {
            return Err(ProtoError::VersionMismatch(
                "a low-level-version challenge should be a tuple that starts with my birthday",
            ));
        }
        if self.macho_version != MACHO_VERSION {
            return Err(ProtoError::VersionMismatch("macho protocol version differs"));
        }
        Ok(())
    }

    pub fn llv_info(&self) -> (i64, f64, i64, Vec<u8>) {
        (self.user_count, self.boot_version, self.boot_build, self.codename.clone())
    }
}

pub fn close_reason(reason: &str, table: &eve_marshal::StringTable) -> Result<Vec<u8>> {
    let bootver = eve_marshal::save(
        &Value::Tuple(vec![
            Value::Double(BOOT_VERSION),
            Value::Int(BOOT_BUILD),
            Value::Bytes(BOOT_CODENAME.to_vec()),
        ]),
        table,
    )?;
    let mut text = reason.as_bytes().to_vec();
    text.extend_from_slice(b", bootver=");
    for b in &bootver {
        text.extend_from_slice(format!("{b:02x}").as_bytes());
    }
    Ok(eve_marshal::save(&Value::Bytes(text), table)?)
}
