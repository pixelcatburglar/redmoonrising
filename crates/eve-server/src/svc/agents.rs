use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::{agent_missions, bound};
use crate::svc::rows::{int, key_val};

const HEADER: &[&str] =
    &["agentID", "divisionID", "corporationID", "stationID", "level", "quality", "agentTypeID",
      "bloodlineID"];

// An agent is also a character; its bloodline lives on that character row
// (the agent window reads a.bloodlineID, agents.py:289).
const SQL: &str = r#"SELECT a."agentID",a."divisionID",a."corporationID",a."stationID",
                            a."level",a."quality",a."agentTypeID",c."bloodlineID"
                       FROM "agtAgents" a
                       LEFT JOIN "character_" c ON c."characterID" = a."agentID"
                      ORDER BY a."agentID""#;

pub async fn agent_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetAgents" => get_agents(ctx).await,
        // All the pilot's missions, or (bound) one agent's.
        "GetMyJournalDetails" => agent_missions::journal(ctx, agent_of(call)).await,
        // Bound per agent: moniker.GetAgent(agentID) (moniker.py:530).
        "DoAction" => agent_missions::conversation(ctx, agent_of(call)?, action_id(call)).await,
        "GetInfoServiceDetails" => info_service_details(ctx, agent_of(call)?).await,
        "GetMissionJournalInfo" => agent_missions::mission_page(ctx, agent_of(call)?).await,
        // Offers are the loyalty point store's, which there is none of.
        "GetOfferJournalInfo" => Value::None,
        // (locationType, locationNumber, warpRange, gang) (menusvc.py:1663).
        "WarpToLocation" => {
            let range = call.args.get(2).and_then(|v| v.as_f64()).unwrap_or(15_000.0);
            return Some(agent_missions::go_to(ctx, call.arg_id(1), range, false).await);
        }
        "GotoLocation" => return Some(agent_missions::go_to(ctx, call.arg_id(1), 0.0, true).await),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

fn agent_of(call: &Call) -> Option<i64> {
    bound::parse(&call.service).and_then(|b| b.params.first().copied())
}

/// DoAction(actionID): None opens the conversation. The agent window's links
/// pass the ID back as text (agents.py:273), so accept either form.
fn action_id(call: &Call) -> Option<i64> {
    call.arg_i64(0).or_else(|| call.arg_text(0)?.trim().parse().ok())
}

/// The agent's Show Info panel (infosvc.py:1404-1429). Effective quality is the
/// plain quality: the skill and standing adjustments are not modelled.
async fn info_service_details(ctx: &Ctx<'_>, agent_id: i64) -> Value {
    let sql = r#"SELECT "stationID","level","quality" FROM "agtAgents" WHERE "agentID" = $1"#;
    let Some(r) = ctx.query_one(sql, &[&(agent_id as i32)]).await else { return Value::None };
    let quality = Value::int_or_none(int(&r, 2));
    key_val(vec![
        ("stationID", Value::int_or_none(int(&r, 0))),
        ("level", Value::int_or_none(int(&r, 1))),
        ("quality", quality.clone()),
        ("effectiveQuality", quality),
        ("loyaltyPoints", Value::Int(0)),
        ("services", Value::List(Vec::new())),
        ("incompatible", Value::None),
    ])
}

async fn get_agents(ctx: &Ctx<'_>) -> Value {
    ctx.rowset(HEADER, SQL, &[]).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::svc::rows::rowset;
    use eve_marshal::StringTable;

    #[test]
    fn the_whole_agent_catalogue_fits_in_one_frame() {
        let agent_lines: Vec<Value> = (0..10_270i64)
            .map(|i| {
                Value::List(vec![
                    Value::Int(3_000_000 + i),
                    Value::Int(i % 8),
                    Value::Int(1_000_000 + i % 200),
                    Value::Int(60_000_000 + i % 5000),
                    Value::Int(i % 5 + 1),
                    Value::Int(i % 20),
                    Value::Int(2),
                    Value::Int(i % 14 + 1),
                ])
            })
            .collect();
        let blob = eve_marshal::save(&rowset(HEADER, agent_lines), &StringTable::empty())
            .expect("the catalogue marshals");
        assert!(
            blob.len() < eve_proto::MAX_FRAME_LEN as usize,
            "GetAgents is {} bytes, over the {} the client silently drops",
            blob.len(),
            eve_proto::MAX_FRAME_LEN
        );
    }
}
