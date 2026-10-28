use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{key_val, rowset};

const COURIER_HEADER: &[&str] = &[
    "missionID", "kind", "state", "availabilityID", "inOrder", "description", "issuerID",
    "assigneeID", "acceptFee", "acceptorID", "dateIssued", "dateExpires", "dateAccepted",
    "dateCompleted", "totalReward", "tracking", "pickupStationID", "craterID", "dropStationID",
    "volume", "pickupSolarSystemID", "pickupRegionID", "dropSolarSystemID", "dropRegionID",
];

const ESCROW_HEADER: &[&str] = &[
    "escrowID", "price", "stationID", "solarsystemID", "regionID", "issuerID", "assigneeID",
    "acceptorID", "dateExpires", "description",
];

pub async fn mission_mgr(call: &Call, _ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetAvailableCourierMissions" | "GetMyCourierMissions" => rowset(COURIER_HEADER, Vec::new()),
        "GetAvailableEscrows" => available_escrows(),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

fn available_escrows() -> Value {
    key_val(vec![
        ("escrows", rowset(ESCROW_HEADER, Vec::new())),
        ("names", Value::Dict(Vec::new())),
    ])
}
