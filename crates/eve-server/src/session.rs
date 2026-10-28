use eve_marshal::Value;

pub const ROLE_LOGIN: i64 = 1;
pub const ROLE_PLAYER: i64 = 2;

pub const CORP_ROLE_DIRECTOR: i64 = 1;
pub const CORP_ROLE_ACCOUNTANT: i64 = 256;

pub const PERSIST_VARS: &[&str] = &[
    "userid", "languageID", "regionid", "constellationid", "role", "charid",
    "allianceid", "corpid", "gangid", "gangrole", "shipid", "stationid",
    "solarsystemid", "solarsystemid2", "address", "callback", "userType",
    "hqID", "baseID", "rolesAtAll", "rolesAtHQ", "rolesAtBase", "rolesAtOther",
];

#[derive(Debug, Clone, Default)]
pub struct Session {
    pub sid: i64,
    pub user_id: Option<i64>,
    pub role: i64,
    pub language_id: Option<String>,
    pub address: Option<String>,
    pub user_type: Option<i64>,
    pub char_id: Option<i64>,

    pub corp_id: Option<i64>,
    pub alliance_id: Option<i64>,
    pub ship_id: Option<i64>,
    pub station_id: Option<i64>,
    pub solar_system_id: Option<i64>,
    pub solar_system_id2: Option<i64>,
    pub constellation_id: Option<i64>,
    pub region_id: Option<i64>,
    pub hq_id: Option<i64>,
    pub base_id: Option<i64>,
    pub roles_at_all: i64,
    pub roles_at_hq: i64,
    pub roles_at_base: i64,
    pub roles_at_other: i64,
    pub gang_id: Option<i64>,
    pub gang_role: Option<i64>,

    /// Not session variables: the selected character's name and type, for
    /// what other players are shown.
    pub char_name: String,
    pub char_type_id: i64,
}

impl Session {
    pub fn new(sid: i64) -> Self {
        Session { sid, role: ROLE_LOGIN, ..Default::default() }
    }

    pub fn corp_role(&self) -> i64 {
        match self.station_id {
            Some(station) if station != 0 && Some(station) == self.hq_id => {
                self.roles_at_all | self.roles_at_hq
            }
            Some(station) if station != 0 && Some(station) == self.base_id => {
                self.roles_at_all | self.roles_at_base
            }
            _ => self.roles_at_all | self.roles_at_other,
        }
    }

    pub fn has_corp_role(&self, mask: i64) -> bool {
        mask != 0 && self.corp_role() & mask == mask
    }

    fn attr(&self, name: &str) -> Value {
        match name {
            "userid" => Value::int_or_none(self.user_id),
            "role" => Value::Long(self.role),
            "languageID" => match &self.language_id {
                Some(l) => Value::Bytes(l.as_bytes().to_vec()),
                None => Value::None,
            },
            "address" => match &self.address {
                Some(a) => Value::Bytes(a.as_bytes().to_vec()),
                None => Value::None,
            },
            "userType" => Value::int_or_none(self.user_type),
            "charid" => Value::int_or_none(self.char_id),
            "corpid" => Value::int_or_none(self.corp_id),
            "allianceid" => Value::int_or_none(self.alliance_id),
            "shipid" => Value::int_or_none(self.ship_id),
            "stationid" => Value::int_or_none(self.station_id),
            "solarsystemid" => Value::int_or_none(self.solar_system_id),
            "solarsystemid2" => Value::int_or_none(self.solar_system_id2),
            "constellationid" => Value::int_or_none(self.constellation_id),
            "regionid" => Value::int_or_none(self.region_id),
            "hqID" => Value::int_or_none(self.hq_id),
            "baseID" => Value::int_or_none(self.base_id),
            "gangid" => Value::int_or_none(self.gang_id),
            "gangrole" => Value::int_or_none(self.gang_role),
            "rolesAtAll" => Value::Long(self.roles_at_all),
            "rolesAtHQ" => Value::Long(self.roles_at_hq),
            "rolesAtBase" => Value::Long(self.roles_at_base),
            "rolesAtOther" => Value::Long(self.roles_at_other),
            _ => Value::None,
        }
    }

    pub fn change_from(&self, old: &Session) -> Value {
        let mut pairs = Vec::new();
        for name in PERSIST_VARS {
            let (was, now) = (old.attr(name), self.attr(name));
            if was != now {
                pairs.push((
                    Value::Bytes(name.as_bytes().to_vec()),
                    Value::Tuple(vec![was, now]),
                ));
            }
        }
        Value::Dict(pairs)
    }

    pub fn change_body_from(&self, old: &Session) -> Vec<Value> {
        vec![
            Value::Tuple(vec![Value::Int(0), self.change_from(old)]),
            Value::List(Vec::new()),
        ]
    }

    pub fn login_change(&self) -> Value {
        let mut pairs = Vec::new();
        for name in PERSIST_VARS {
            let new = self.attr(name);
            let old = default_for(name);
            if new != old {
                pairs.push((
                    Value::Bytes(name.as_bytes().to_vec()),
                    Value::Tuple(vec![old, new]),
                ));
            }
        }
        Value::Dict(pairs)
    }

    pub fn change_body(&self) -> Vec<Value> {
        vec![
            Value::Tuple(vec![Value::Int(0), self.login_change()]),
            Value::List(Vec::new()),
        ]
    }
}


fn default_for(name: &str) -> Value {
    match name {
        "role" => Value::Long(ROLE_LOGIN),
        "rolesAtAll" | "rolesAtHQ" | "rolesAtBase" | "rolesAtOther" => Value::Long(0),
        _ => Value::None,
    }
}
