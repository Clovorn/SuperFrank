//! FrankOS Agent Swarm — spawn, coordinate, and collect worker agents

use anyhow::Result;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::Arc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::llm::{ChatMessage, LlmClient, LlmProvider};
use crate::tools::{all_tools, execute_tool, to_anthropic_tools, ToolContext};
use crate::identity;

/// Model name → Anthropic model string
fn resolve_model(model_hint: &str) -> &'static str {
    match model_hint {
        "haiku"  | "fast"   | "cheap"  => "claude-haiku-4-5",
        "sonnet" | "normal" | "medium" => "claude-sonnet-4-5",
        "opus"   | "deep"   | "heavy"  => "claude-opus-4-5",
        _ => "claude-haiku-4-5",
    }
}

/// Run a worker agent to completion — called in a Tokio background task
pub async fn run_agent(
    db: PgPool,
    llm: Arc<LlmClient>,
    agent_id: Uuid,
    brave_key: Option<String>,
) {
    info!("Agent {} starting", agent_id);

    // Mark running
    let _ = sqlx::query(
        "UPDATE frankos_agents SET status = 'running', started_at = NOW() WHERE id = $1"
    ).bind(agent_id).execute(&db).await;

    match run_agent_inner(&db, &llm, agent_id, brave_key).await {
        Ok(result) => {
            info!("Agent {} completed: {}", agent_id, &result[..result.len().min(100)]);
            let _ = sqlx::query(
                "UPDATE frankos_agents SET status = 'complete', result = $1, completed_at = NOW() WHERE id = $2"
            ).bind(&result).bind(agent_id).execute(&db).await;
        }
        Err(e) => {
            warn!("Agent {} failed: {}", agent_id, e);
            let _ = sqlx::query(
                "UPDATE frankos_agents SET status = 'failed', error = $1, completed_at = NOW() WHERE id = $2"
            ).bind(e.to_string()).bind(agent_id).execute(&db).await;
        }
    }
}

async fn run_agent_inner(
    db: &PgPool,
    llm: &LlmClient,
    agent_id: Uuid,
    brave_key: Option<String>,
) -> Result<String> {
    use sqlx::Row;

    // Load agent record
    let row = sqlx::query(
        "SELECT name, goal, tools_allowed, model, parent_session_id, user_id FROM frankos_agents WHERE id = $1"
    ).bind(agent_id).fetch_one(db).await?;

    let name: String = row.try_get("name")?;
    let goal: String = row.try_get("goal")?;
    let model_hint: String = row.try_get("model").unwrap_or_else(|_| "haiku".into());
    let parent_session_id: Uuid = row.try_get("parent_session_id")?;
    let user_id: Uuid = row.try_get("user_id")?;
    let tools_json: Value = row.try_get("tools_allowed").unwrap_or(json!([]));

    let allowed_tools: Vec<String> = tools_json.as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let model = resolve_model(&model_hint);

    // Build tool list for this agent
    let available_tools = if allowed_tools.is_empty() {
        all_tools()
    } else {
        all_tools().into_iter().filter(|t| allowed_tools.contains(&t.name)).collect()
    };

    let tool_ctx = ToolContext {
        brave_api_key: brave_key,
        google_ai_key: None,
        google_ai_project: None,
        luma_api_key: None,
        openai_api_key: None,
        db: db.clone(),
        session_id: parent_session_id,
        user_id,
        chat_bucket: "personal".to_string(),
        chat_folder: None,
        forge: None,
    };

    let system = format!(
        r#"You are {name}, a FrankOS worker agent.

Your goal: {goal}

You have access to tools to accomplish this goal. Use them as needed.
Work efficiently — don't call tools you don't need.
When you have completed the goal, provide a clear summary of what you accomplished.
Be concise in your final response — SuperFrank will synthesize your results."#,
        name = name,
        goal = goal,
    );

    let anthropic_tools = to_anthropic_tools(&available_tools);
    let mut messages: Vec<ChatMessage> = vec![
        ChatMessage { role: "user".to_string(), content: goal.clone() }
    ];

    // Agentic tool loop — up to 10 iterations
    for iteration in 0..10 {
        info!("Agent {} iteration {}", agent_id, iteration);

        let response = llm.complete_with_tools(
            "claude-haiku-4-5",
            &system,
            &messages,
            8192,
            &anthropic_tools,
        ).await?;

        // Log iteration to DB
        let _ = sqlx::query(
            "UPDATE frankos_agents SET iterations = iterations + 1 WHERE id = $1"
        ).bind(agent_id).execute(db).await;

        match response {
            AgentResponse::Text(text) => {
                // Agent is done
                return Ok(text);
            }
            AgentResponse::ToolUse(tool_calls) => {
                // Add assistant message with tool use blocks
                let assistant_content: Vec<Value> = tool_calls.iter().map(|tc| json!({
                    "type": "tool_use",
                    "id": tc.id,
                    "name": tc.name,
                    "input": tc.input,
                })).collect();

                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: serde_json::to_string(&assistant_content).unwrap_or_default(),
                });

                // Execute all tools
                let mut tool_results: Vec<Value> = Vec::new();
                for tc in &tool_calls {
                    info!("Agent {} calling tool: {}", agent_id, tc.name);

                    // Log tool call
                    let _ = sqlx::query(
                        "INSERT INTO frankos_agent_tool_calls (agent_id, tool_name, input) VALUES ($1, $2, $3)"
                    ).bind(agent_id).bind(&tc.name).bind(&tc.input).execute(db).await;

                    let result = execute_tool(&tc.name, &tc.input, &tool_ctx).await;

                    // Log tool result
                    let _ = sqlx::query(
                        "UPDATE frankos_agent_tool_calls SET output = $1, success = $2, completed_at = NOW() WHERE agent_id = $3 AND tool_name = $4 AND completed_at IS NULL"
                    ).bind(&result.output).bind(result.success).bind(agent_id).bind(&tc.name).execute(db).await;

                    tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": tc.id,
                        "content": serde_json::to_string(&result.output).unwrap_or_default(),
                    }));
                }

                // Add tool results as user message
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: serde_json::to_string(&tool_results).unwrap_or_default(),
                });
            }
        }
    }

    Ok("Agent reached maximum iterations without completing goal.".to_string())
}

// ── Response types from LLM ───────────────────────────────────────────────────

#[derive(Debug)]
pub enum AgentResponse {
    Text(String),
    ToolUse(Vec<ToolCall>),
}

#[derive(Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

// ── Background agent spawner ──────────────────────────────────────────────────

/// Spawn pending agents found in the DB — call periodically or after new agent inserted
pub async fn spawn_pending_agents(
    db: &PgPool,
    llm: Arc<LlmClient>,
    brave_key: Option<String>,
) {
    use sqlx::Row;

    let rows = sqlx::query(
        "SELECT id FROM frankos_agents WHERE status = 'pending' ORDER BY created_at ASC LIMIT 10"
    ).fetch_all(db).await.unwrap_or_default();

    for row in rows {
        let agent_id: Uuid = row.try_get("id").unwrap_or_default();
        let db2 = db.clone();
        let llm2 = llm.clone();
        let key2 = brave_key.clone();

        tokio::spawn(async move {
            run_agent(db2, llm2, agent_id, key2).await;
        });
    }
}
