use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::bound;
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

/// The conversation's one dialogue choice (action IDs are the server's own).
const ASK_FOR_WORK: i64 = 1;

pub async fn agent_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetAgents" => get_agents(ctx).await,
        "GetMyJournalDetails" => my_journal_details(),
        // Bound per agent: moniker.GetAgent(agentID) (moniker.py:530).
        "DoAction" => do_action(ctx, action_id(call)),
        "GetInfoServiceDetails" => info_service_details(ctx, agent_of(call)?).await,
        // No missions yet, so no journal pages; agents.py opens nothing on None.
        "GetMissionJournalInfo" | "GetOfferJournalInfo" => Value::None,
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

/// ((agentSays, [(actionID, choiceText)]), oob), unpacked by
/// agents.py __GetConversation. There is no mission engine yet, so every agent
/// has the same short exchange.
fn do_action(ctx: &Ctx<'_>, action: Option<i64>) -> Value {
    let (says, choices) = match action {
        Some(ASK_FOR_WORK) => (
            "I'm sorry, I don't have any work for you at the moment. Check back with me later."
                .to_owned(),
            Vec::new(),
        ),
        _ => (
            format!("Hello, {}. What can I do for you?", html_escape(&ctx.session.char_name)),
            vec![Value::Tuple(vec![
                Value::Int(ASK_FOR_WORK),
                Value::text("Do you have any work for me?"),
            ])],
        ),
    };
    Value::Tuple(vec![
        Value::Tuple(vec![Value::text(says), Value::List(choices)]),
        Value::Dict(Vec::new()),
    ])
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
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

fn my_journal_details() -> Value {
    Value::List(vec![
        Value::List(Vec::new()),
        Value::List(Vec::new()),
        Value::List(Vec::new()),
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
