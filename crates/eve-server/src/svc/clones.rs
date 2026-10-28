use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::rowset;

const SERVICE: &str = "jumpCloneSvc";

const CLONE_HEADER: &[&str] = &["jumpCloneID", "locationID"];

const IMPLANT_HEADER: &[&str] = &["jumpCloneID", "typeID"];

const SHIP_CLONE_HEADER: &[&str] = &["jumpCloneID", "ownerID", "locationID"];

pub async fn jump_clone_svc(call: &Call, _ctx: &Ctx<'_>) -> Option<Outcome> {
    Some(jump_clone(&call.method, &call.args))
}

fn jump_clone(method: &str, args: &[Value]) -> Outcome {
    let out = match method {
        "GetCloneState" => Value::Tuple(vec![
            rowset(CLONE_HEADER, Vec::new()),
            rowset(IMPLANT_HEADER, Vec::new()),
        ]),

        "GetShipCloneState" | "GetStationCloneState" => {
            rowset(SHIP_CLONE_HEADER, Vec::new())
        }

        "GetPriceForClone" => Value::Double(0.0),

        "InstallCloneInStation" | "DestroyInstalledClone" | "CloneJump"
        | "OfferShipCloneInstallation" | "AcceptShipCloneInstallation"
        | "CancelShipCloneInstallation" => {
            return Outcome::Unknown(format!(
                "{SERVICE}::{method}{} needs the clone tables \
                 (db/migrations/0012_insurance_clones_bounties.sql, not yet applied)",
                arg_summary(args)
            ))
        }

        _ => return Outcome::Unknown(format!("{SERVICE}::{method}")),
    };
    Outcome::Value(out)
}

fn arg_summary(args: &[Value]) -> String {
    let ids: Vec<String> =
        args.iter().filter_map(|v| v.as_i64()).map(|v| v.to_string()).collect();
    if ids.is_empty() {
        String::new()
    } else {
        format!("({})", ids.join(", "))
    }
}
