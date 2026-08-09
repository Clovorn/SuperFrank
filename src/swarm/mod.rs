//! The Swarm — SuperFrank's parallel agent mesh.
//!
//! v2 agents: isolated, sequential, 10-iteration cap, no visibility until done.
//! v3 swarm:
//!   - Parallel execution via FuturesUnordered
//!   - Agent-to-agent mailbox (researchers hand off to coders, etc.)
//!   - Every iteration streams progress back to Chuck in real time
//!   - Complexity-based model routing (Haiku/Sonnet/Opus)
//!   - Token budget instead of iteration cap
//!   - Nexus-triggered agents (run on schedule, no user prompt needed)
//!   - apply_patch tool for atomic multi-file edits

use anyhow::{anyhow, Result};
use futures::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::delivery::DeliveryBus;
use crate::llm::{ChatMessage, LlmClient, StreamEvent};
use crate::tools::{all_tools, execute_tool, to_anthropic_tools, ToolContext};

// ── Model routing ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Complexity {
    Lookup,    // Simple fetch / answer → Haiku
    Standard,  // Normal task → Sonnet
    Deep,      // Reasoning-heavy → Opus
    Code,      // Code generation / review → Sonnet
    Build,     // Compile / deploy → Sonnet (has shell tools)
}

pub fn route_model(complexity: &Complexity) -> &'static str {
    match complexity {
        Complexity::Lookup   => "claude-haiku-4-5",
        Complexity::Standard => "claude-sonnet-4-5",
        Complexity::Deep     => "claude-opus-4-5",
        Complexity::Code     => "claude-sonnet-4-5",
        Complexity::Build    => "claude-sonnet-4-5",
    }
}

/// Quick complexity classification using a tiny Haiku call.
/// Returns the model string directly.
pub async fn classify_and_route(
    llm: &LlmClient,
    goal: &str,
) -> &'static str {
    let prompt = format!(
        "Classify the complexity of this task with ONE word: \
         lookup, standard, deep, code, or build.\n\nTask: {}\n\nReply with only one word.",
        &goal[..goal.len().min(300)]
    );
    let classification = llm
        .complete_simple("claude-haiku-4-5", &prompt, 10)
        .await
        .unwrap_or_default()
        .trim()
        .to_lowercase();

    match classification.as_str() {
        "lookup"   => "claude-haiku-4-5",
        "standard" => "claude-sonnet-4-5",
        "deep"     => "claude-opus-4-5",
        "code"     => "claude-sonnet-4-5",
        "build"    => "claude-sonnet-4-5",
        _          => "claude-sonnet-4-5", // safe default
    }
}

// ── Swarm ─────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Swarm {
    pub db: PgPool,
    pub llm: Arc<LlmClient>,
}

impl Swarm {
    pub fn new(db: PgPool, llm: Arc<LlmClient>) -> Self {
        Self { db, llm }
    }

    /// Spawn multiple agents in parallel. Returns when ALL complete.
    /// Progress streams to `tx` as each agent produces output.
    pub async fn spawn_parallel(
        &self,
        tasks: Vec<SwarmTask>,
        tx: mpsc::Sender<SwarmEvent>,
        tool_ctx: ToolContext,
    ) -> Vec<SwarmResult> {
        let mut futures = FuturesUnordered::new();

        for task in tasks {
            let swarm = self.clone();
            let tx2 = tx.clone();
            let ctx2 = tool_ctx.clone();
            futures.push(tokio::spawn(async move {
                swarm.run_single(task, tx2, ctx2).await
            }));
        }

        let mut results = Vec::new();
        while let Some(result) = futures.next().await {
            match result {
                Ok(r) => results.push(r),
                Err(e) => warn!("[Swarm] Task join error: {}", e),
            }
        }
        results
    }

    /// Run a single agent to completion (used by both parallel spawner and direct spawn_agent tool).
    pub async fn run_single(
        &self,
        task: SwarmTask,
        tx: mpsc::Sender<SwarmEvent>,
        tool_ctx: ToolContext,
    ) -> SwarmResult {
        let model = task.model.as_deref()
            .unwrap_or_else(|| classify_and_route_sync(&task.goal));

        info!("[Swarm] Agent '{}' starting on model={}", task.name, model);

        // Write to DB
        let agent_id = match self.create_agent_record(&task, model).await {
            Ok(id) => id,
            Err(e) => {
                warn!("[Swarm] Failed to create agent record: {}", e);
                Uuid::new_v4()
            }
        };

        let _ = tx.send(SwarmEvent::AgentStarted {
            agent_id,
            name: task.name.clone(),
            model: model.to_string(),
        }).await;

        let system = build_agent_system(&task.name, &task.goal, &task.context);
        let available_tools = match &task.tools {
            Some(allowed) if !allowed.is_empty() => {
                all_tools().into_iter()
                    .filter(|t| allowed.contains(&t.name))
                    .collect()
            }
            _ => all_tools(),
        };
        let anthropic_tools = to_anthropic_tools(&available_tools);

        let mut messages: Vec<ChatMessage> = vec![
            ChatMessage { role: "user".to_string(), content: task.goal.clone() }
        ];

        let mut tokens_used: i32 = 0;
        let token_budget = task.token_budget.unwrap_or(80_000) as i32;
        let mut iteration = 0u32;
        let mut final_result = String::new();

        loop {
            iteration += 1;
            tokens_used += estimate_tokens(&messages);

            if tokens_used >= token_budget {
                warn!("[Swarm] Agent '{}' hit token budget ({} tokens)", task.name, tokens_used);
                final_result = format!(
                    "[Stopped: token budget of {} reached after {} iterations. \
                     Partial work may be incomplete.]",
                    token_budget, iteration - 1
                );
                break;
            }

            let _ = tx.send(SwarmEvent::AgentIteration {
                agent_id,
                name: task.name.clone(),
                iteration,
                tokens_used,
            }).await;

            // Update iteration count in DB
            let _ = sqlx::query(
                "UPDATE frankos_agents SET iterations = $1 WHERE id = $2"
            ).bind(iteration as i32).bind(agent_id).execute(&self.db).await;

            // LLM call with tools
            let (event_tx, mut event_rx) = mpsc::channel::<StreamEvent>(256);
            let llm = self.llm.clone();
            let model_str = model.to_string();
            let sys_clone = system.clone();
            let msgs_clone = messages.clone();
            let tools_clone = anthropic_tools.clone();

            let call_handle = tokio::spawn(async move {
                llm.stream_with_tools_and_calls(
                    &crate::llm::LlmProvider::Anthropic,
                    &model_str,
                    &sys_clone,
                    msgs_clone,
                    8192,
                    &tools_clone,
                    event_tx,
                ).await
            });

            // Forward streaming events to parent SSE
            let mut tool_calls_this_turn = Vec::new();
            let mut text_this_turn = String::new();

            while let Some(event) = event_rx.recv().await {
                match &event {
                    StreamEvent::Delta(text) => {
                        // Strip JSON encoding from delta (it comes JSON-encoded)
                        let decoded = serde_json::from_str::<String>(text).unwrap_or_else(|_| text.clone());
                        text_this_turn.push_str(&decoded);
                        let _ = tx.send(SwarmEvent::AgentDelta {
                            agent_id,
                            name: task.name.clone(),
                            text: decoded,
                        }).await;
                    }
                    StreamEvent::ToolStart { id, name } => {
                        let _ = tx.send(SwarmEvent::AgentToolStart {
                            agent_id,
                            tool_name: name.clone(),
                            tool_call_id: id.clone(),
                        }).await;
                    }
                    StreamEvent::ToolInput { id, name, input } => {
                        tool_calls_this_turn.push((id.clone(), name.clone(), input.clone()));
                    }
                    StreamEvent::ToolResult { .. } => {}
                    StreamEvent::Iteration { .. } => {}
                    StreamEvent::Notification { .. } => {}
                StreamEvent::Notification { .. } => {}
            StreamEvent::Done => break,
                }
            }

            let llm_result = call_handle.await;
            let (_, tool_requests) = match llm_result {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    warn!("[Swarm] LLM error for '{}': {}", task.name, e);
                    final_result = format!("LLM error: {}", e);
                    break;
                }
                Err(e) => {
                    warn!("[Swarm] Join error: {}", e);
                    break;
                }
            };

            if tool_requests.is_empty() {
                // No tools — agent is done
                final_result = text_this_turn;
                break;
            }

            // Build assistant message with tool_use blocks
            let assistant_blocks: Vec<Value> = tool_requests.iter().map(|tc| json!({
                "type": "tool_use",
                "id": tc.id,
                "name": tc.name,
                "input": tc.input,
            })).collect();
            messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: serde_json::to_string(&assistant_blocks).unwrap_or_default(),
            });

            // Execute tools — in parallel if they don't have data dependencies
            let mut tool_results: Vec<Value> = Vec::new();
            for tc in &tool_requests {
                let _ = sqlx::query(
                    "INSERT INTO frankos_agent_tool_calls (agent_id, tool_name, input) VALUES ($1, $2, $3)"
                ).bind(agent_id).bind(&tc.name).bind(&tc.input).execute(&self.db).await;

                let result = execute_tool(&tc.name, &tc.input, &tool_ctx).await;

                let _ = tx.send(SwarmEvent::AgentToolResult {
                    agent_id,
                    tool_name: tc.name.clone(),
                    success: result.success,
                    output_preview: result.output.as_str()
                        .map(|s| s[..s.len().min(200)].to_string())
                        .unwrap_or_else(|| result.output.to_string()[..200.min(result.output.to_string().len())].to_string()),
                }).await;

                let _ = sqlx::query(
                    "UPDATE frankos_agent_tool_calls SET output = $1, success = $2, completed_at = NOW()
                     WHERE agent_id = $3 AND tool_name = $4 AND completed_at IS NULL"
                ).bind(&result.output).bind(result.success).bind(agent_id).bind(&tc.name)
                 .execute(&self.db).await;

                tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tc.id,
                    "content": result.output.to_string(),
                }));
            }

            messages.push(ChatMessage {
                role: "user".to_string(),
                content: serde_json::to_string(&tool_results).unwrap_or_default(),
            });
        }

        // Write final result to DB
        let _ = sqlx::query(
            "UPDATE frankos_agents SET status = 'complete', result = $1, completed_at = NOW() WHERE id = $2"
        ).bind(&final_result).bind(agent_id).execute(&self.db).await;

        let _ = tx.send(SwarmEvent::AgentCompleted {
            agent_id,
            name: task.name.clone(),
            result: final_result.clone(),
        }).await;

        info!("[Swarm] Agent '{}' completed in {} iterations", task.name, iteration);

        SwarmResult {
            agent_id,
            name: task.name,
            result: final_result,
            iterations: iteration,
            tokens_used,
        }
    }

    /// Spawn an agent triggered by the Nexus (no live SSE session — delivers via DeliveryBus).
    pub async fn spawn_nexus_agent(
        &self,
        trigger_id: Uuid,
        name: &str,
        prompt: &str,
        model_hint: &str,
        tools: &[String],
        user_id: Uuid,
        delivery: Arc<DeliveryBus>,
    ) -> Result<()> {
        let db = self.db.clone();
        let llm = self.llm.clone();
        let swarm = self.clone();
        let name = name.to_string();
        let prompt = prompt.to_string();
        let model = resolve_model_hint(model_hint).to_string();
        let tools_owned: Vec<String> = tools.to_vec();

        tokio::spawn(async move {
            let (tx, mut rx) = mpsc::channel::<SwarmEvent>(256);

            // Discard progress events — this is a background agent
            tokio::spawn(async move { while rx.recv().await.is_some() {} });

            let task = SwarmTask {
                name: name.clone(),
                goal: prompt.clone(),
                model: Some(model),
                tools: if tools_owned.is_empty() { None } else { Some(tools_owned) },
                context: Some(format!("Triggered by Nexus trigger_id={}", trigger_id)),
                token_budget: Some(80_000),
            };

            let tool_ctx = ToolContext {
                brave_api_key: None,
                google_ai_key: None,
                google_ai_project: None,
                luma_api_key: None,
                openai_api_key: None,
                db: db.clone(),
                session_id: Uuid::nil(),
                user_id,
                chat_bucket: "personal".to_string(),
                chat_folder: None,
        forge: None,
            };

            let result = swarm.run_single(task, tx, tool_ctx).await;

            // Deliver result to user
            let truncated = result.result.chars().take(500).collect::<String>();
            let _ = delivery.notify_user(
                user_id,
                &format!("✅ {}", name),
                &truncated,
            ).await;
        });

        Ok(())
    }

    async fn create_agent_record(&self, task: &SwarmTask, model: &str) -> Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO frankos_agents (id, name, goal, model, tools_allowed, status)
             VALUES ($1, $2, $3, $4, $5, 'running')"
        )
        .bind(id)
        .bind(&task.name)
        .bind(&task.goal)
        .bind(model)
        .bind(json!(task.tools.as_deref().unwrap_or(&[])))
        .execute(&self.db)
        .await?;
        Ok(id)
    }
}

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SwarmTask {
    pub name: String,
    pub goal: String,
    pub model: Option<String>,
    pub tools: Option<Vec<String>>,
    pub context: Option<String>,
    pub token_budget: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct SwarmResult {
    pub agent_id: Uuid,
    pub name: String,
    pub result: String,
    pub iterations: u32,
    pub tokens_used: i32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SwarmEvent {
    AgentStarted { agent_id: Uuid, name: String, model: String },
    AgentIteration { agent_id: Uuid, name: String, iteration: u32, tokens_used: i32 },
    AgentDelta { agent_id: Uuid, name: String, text: String },
    AgentToolStart { agent_id: Uuid, tool_name: String, tool_call_id: String },
    AgentToolResult { agent_id: Uuid, tool_name: String, success: bool, output_preview: String },
    AgentCompleted { agent_id: Uuid, name: String, result: String },
    AgentFailed { agent_id: Uuid, name: String, error: String },
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn classify_and_route_sync(goal: &str) -> &'static str {
    // Sync heuristic for when we can't afford an extra LLM call
    let g = goal.to_lowercase();
    if g.contains("build") || g.contains("compile") || g.contains("deploy") || g.contains("cargo") {
        "claude-sonnet-4-5"
    } else if g.contains("reason") || g.contains("analyze") || g.contains("design") || g.contains("architect") {
        "claude-opus-4-5"
    } else if g.len() < 200 && !g.contains("write") && !g.contains("implement") {
        "claude-haiku-4-5"
    } else {
        "claude-sonnet-4-5"
    }
}

fn resolve_model_hint(hint: &str) -> &'static str {
    match hint {
        "haiku" | "fast" | "cheap" | "lookup" => "claude-haiku-4-5",
        "sonnet" | "standard" | "normal" => "claude-sonnet-4-5",
        "opus" | "deep" | "heavy" => "claude-opus-4-5",
        _ => "claude-sonnet-4-5",
    }
}

fn build_agent_system(name: &str, goal: &str, context: &Option<String>) -> String {
    let ctx = context.as_deref().unwrap_or("");
    format!(
        r#"You are {name}, a SuperFrank worker agent.

Your goal: {goal}
{context_section}
Use your tools efficiently. When the goal is complete, provide a clear, concise summary of what you accomplished.
Do not pad your final response — SuperFrank synthesizes your results for Chuck."#,
        name = name,
        goal = goal,
        context_section = if ctx.is_empty() { String::new() } else { format!("\nContext: {}\n", ctx) },
    )
}

/// Rough token estimator — 4 chars ≈ 1 token
fn estimate_tokens(messages: &[ChatMessage]) -> i32 {
    messages.iter().map(|m| (m.content.len() / 4) as i32).sum()
}
