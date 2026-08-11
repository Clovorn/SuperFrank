//! Server-Sent Events streaming endpoint — with tool execution and message persistence

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{sse::{Event, KeepAlive, Sse}, IntoResponse},
    routing::post,
    Json, Router,
};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::{
    agents::spawn_pending_agents,
    identity,
    llm::{ChatMessage, LlmProvider, StreamEvent},
    memory,
    tools::{all_tools_v3, to_anthropic_tools, ToolContext},
    AppState,
};

/// Score a message at write time — returns (importance, message_type, is_handoff)
fn score_message(content: &str) -> (i32, String, bool) {
    let lower = content.to_lowercase();
    let first_100 = content.chars().take(100).collect::<String>().to_lowercase();
    
    // Importance 10: Handoff nodes (session boundaries, critical status reports)
    if first_100.starts_with("frank_to_mac")
        || first_100.starts_with("mac frank here")
        || first_100.starts_with("morning briefing")
        || first_100.starts_with("complete:")
        || first_100.starts_with("blocked:")
        || first_100.starts_with("deployed:")
        || first_100.starts_with("session handoff")
    {
        return (10, "handoff".to_string(), true);
    }
    
    // Importance 9: Milestones (completions, successful deployments, achievements)
    if lower.contains("deployed") || lower.contains("build succeeded") 
        || lower.contains("migration complete") || lower.contains(" complete")
        || lower.contains("v9 live") || lower.contains("all tests pass")
    {
        return (9, "milestone".to_string(), false);
    }
    
    // Importance 8: Decisions (architecture choices, confirmations, approvals)
    if lower.contains("decided") || lower.contains("architecture")
        || lower.contains("spec complete") || lower.contains("approved")
        || lower.contains("confirmed") || lower.contains("going with")
    {
        return (8, "decision".to_string(), false);
    }
    
    // Importance 7: Blockers (errors, failures, constraints)
    if lower.contains("blocked") || lower.contains("error:")
        || lower.contains("failed:") || lower.contains("fk constraint")
        || lower.contains("compile error")
    {
        return (7, "blocker".to_string(), false);
    }
    
    // Importance 3: Working/status updates (short responses, checking messages)
    if content.len() < 120 
        || lower.contains("let me check") || lower.contains("checking")
        || lower.contains("one moment") || lower.contains("looking at")
    {
        return (3, "working".to_string(), false);
    }
    
    // Default: Normal conversation
    (5, "conversation".to_string(), false)
}

pub fn sse_router() -> Router<AppState> {
    Router::new()
        .route("/chat/sessions/:id/stream", post(stream_message))
}

#[derive(Deserialize)]
struct StreamRequest {
    content: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
}


fn parse_context_hint(content: &str) -> (String, Option<String>) {
    if let Some(rest) = content.strip_prefix("[Context: ") {
        if let Some(end) = rest.find(']') {
            let hint = &rest[..end];
            let parts: Vec<&str> = hint.splitn(2, " › ").collect();
            let bucket = parts[0].trim().to_lowercase();
            let folder = parts.get(1).map(|s| s.trim().to_string());
            return (bucket, folder);
        }
    }
    ("personal".to_string(), None)
}

fn strip_context_hint(content: &str) -> String {
    if content.starts_with("[Context: ") {
        if let Some(pos) = content.find("]\n") {
            return content[pos + 2..].to_string();
        }
        if let Some(pos) = content.find(']') {
            return content[pos + 1..].trim_start().to_string();
        }
    }
    content.to_string()
}

async fn stream_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<Uuid>,
    Json(req): Json<StreamRequest>,
) -> impl IntoResponse {
    let (user_id, email, _role) = match crate::routes::extract_user_pub(&headers, &state.config.jwt_secret) {
        Some(u) => u,
        None => return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"})))),
    };

    // Parse context hint from frontend before storing
    let (chat_bucket, chat_folder) = parse_context_hint(&req.content);
    let clean_content = strip_context_hint(&req.content);

    // Score the user message
    let (importance, message_type, is_handoff) = score_message(&clean_content);

    // Store user message (stripped of context hint)
    let _ = sqlx::query(
        "INSERT INTO frankos_messages (session_id, user_id, role, content, importance, message_type, is_handoff) 
         VALUES ($1, $2, 'user', $3, $4, $5, $6)"
    ).bind(session_id).bind(user_id).bind(&clean_content)
     .bind(importance).bind(&message_type).bind(is_handoff)
     .execute(&state.db).await;

    // Load conversation history with wave-aware loader
    // Three queries: (A) all handoffs, (B) top 10 peaks (importance 8+), (C) last 15 by recency
    // Deduplicate by ID, sort chronologically
    
    use std::collections::HashMap;
    
    #[derive(Clone)]
    struct MessageRow {
        id: Uuid,
        role: String,
        content: String,
        created_at: chrono::DateTime<chrono::Utc>,
    }
    
    // Query A: All handoff nodes from entire session
    let handoff_rows = sqlx::query(
        "SELECT id, role, content, created_at FROM frankos_messages 
         WHERE session_id = $1 AND user_id = $2 AND is_handoff = true 
         ORDER BY created_at ASC"
    ).bind(session_id).bind(user_id).fetch_all(&state.db).await.unwrap_or_default();
    
    // Query B: Top 10 peaks (importance 8+, non-handoff)
    let peak_rows = sqlx::query(
        "SELECT id, role, content, created_at FROM frankos_messages 
         WHERE session_id = $1 AND user_id = $2 AND importance >= 8 AND is_handoff = false 
         ORDER BY importance DESC, created_at ASC LIMIT 10"
    ).bind(session_id).bind(user_id).fetch_all(&state.db).await.unwrap_or_default();
    
    // Query C: Last 15 messages by recency
    let recent_rows = sqlx::query(
        "SELECT id, role, content, created_at FROM frankos_messages 
         WHERE session_id = $1 AND user_id = $2 
         ORDER BY created_at DESC LIMIT 15"
    ).bind(session_id).bind(user_id).fetch_all(&state.db).await.unwrap_or_default();
    
    // Merge and deduplicate
    let mut msg_map: HashMap<Uuid, MessageRow> = HashMap::new();
    
    for row in handoff_rows.iter().chain(peak_rows.iter()).chain(recent_rows.iter()) {
        let id: Uuid = row.try_get("id").unwrap();
        let role: String = row.try_get("role").unwrap_or_default();
        let content: String = row.try_get("content").unwrap_or_default();
        let created_at: chrono::DateTime<chrono::Utc> = row.try_get("created_at").unwrap();
        
        msg_map.entry(id).or_insert(MessageRow { id, role, content, created_at });
    }
    
    // Sort by created_at
    let mut all_msgs: Vec<MessageRow> = msg_map.into_values().collect();
    all_msgs.sort_by_key(|m| m.created_at);
    
    use sqlx::Row;
    // Filter to plain-text messages only — tool_use/tool_result blocks are ephemeral
    // and must NOT be replayed into Anthropic (causes 400: unexpected tool_use_id).
    // Only the final text response is persisted; tool exchange turns are in-memory only.
    let messages: Vec<ChatMessage> = all_msgs.iter().filter_map(|r| {
        let role = &r.role;
        let content = &r.content;
        // Strictly filter out ANY message that contains tool exchange content.
        // Tool turns are ephemeral and must NEVER be replayed into Anthropic
        // — causes 400: unexpected tool_use_id. Only keep plain-text messages.
        let trimmed = content.trim();
        let has_tool_content = trimmed.starts_with('[')
            || trimmed.starts_with('{')
            || content.contains(r#""tool_use""#)
            || content.contains(r#""tool_result""#)
            || content.contains(r#""tool_use_id""#);
        if has_tool_content {
            None
        } else {
            Some(ChatMessage { role: role.clone(), content: content.clone() })
        }
    }).collect();

    // Recall memory context
    let recall_ctx = memory::recall(&state.db, "chuck_frank", None, 5).await.unwrap_or_default();
    let memory_block = recall_ctx.to_context_block();

    // Get user name
    let user_name_row = sqlx::query("SELECT name FROM frankos_users WHERE id = $1")
        .bind(user_id).fetch_optional(&state.db).await.ok().flatten();
    let user_name = user_name_row
        .and_then(|r| r.try_get::<String, _>("name").ok())
        .unwrap_or_else(|| email.split('@').next().unwrap_or("Friend").to_string());

    // Load active goals for system prompt injection (Gap 2)
    let active_goals = crate::goals_tools::load_active_goals_for_prompt(&state.db, user_id).await;
    
    // Gap 7C — Blueprint-check pattern: semantic search for relevant context before building
    let blueprint_ctx = if let Some(openai_key) = &state.config.openai_api_key {
        // Semantic search using the user's message directly
        match crate::semantic_search::semantic_search(
            &state.db,
            &clean_content,
            "chuck_frank",
            openai_key,
            5,
            Some(0.4),
            None,
        ).await {
            Ok(results) if !results.is_empty() => {
                let mut parts = vec!["## Blueprint Context (retrieved from memory)".to_string()];
                for r in results {
                    parts.push(format!("- **{}** [{}]: {}", r.title, r.memory_type.unwrap_or_default(), 
                        r.content.chars().take(300).collect::<String>()));
                }
                parts.join("\n")
            }
            _ => String::new()
        }
    } else {
        String::new()
    };
    
    // Gap 10B — Load unread mailbox messages for SuperFrank
    let mailbox_ctx = match sqlx::query_as::<_, (uuid::Uuid, Option<uuid::Uuid>, String, String, String, chrono::DateTime<chrono::Utc>)>(
        "SELECT id, from_agent_id, message_type, subject, content, created_at
         FROM frank_agent_mailbox
         WHERE (to_agent_id IS NULL OR to_agent_id = $1) AND status = 'unread'
         ORDER BY created_at ASC LIMIT 10"
    ).bind(user_id).fetch_all(&state.db).await {
        Ok(msgs) if !msgs.is_empty() => {
            let mut parts = vec!["## Unread Escalations".to_string()];
            for (id, from_id, msg_type, subject, content, created) in &msgs {
                parts.push(format!(
                    "\n**[{}]** {} — {}\n\nFrom: {}\nCreated: {}\n\n{}",
                    msg_type, subject, id,
                    from_id.map(|u| u.to_string()).unwrap_or_else(|| "System".to_string()),
                    created.format("%Y-%m-%d %H:%M UTC"),
                    content
                ));
            }
            
            // Mark them as read after loading into context
            let ids: Vec<uuid::Uuid> = msgs.iter().map(|(id, _, _, _, _, _)| *id).collect();
            let _ = sqlx::query("UPDATE frank_agent_mailbox SET status = 'read' WHERE id = ANY($1)")
                .bind(&ids)
                .execute(&state.db)
                .await;
            
            parts.join("\n")
        }
        _ => String::new()
    };
    
    // Combine blueprint and mailbox context
    let combined_ctx = if mailbox_ctx.is_empty() {
        blueprint_ctx
    } else if blueprint_ctx.is_empty() {
        mailbox_ctx
    } else {
        format!("{}\n\n{}", blueprint_ctx, mailbox_ctx)
    };
    
    let system = identity::system_prompt_with_goals(&user_name, "user", &memory_block, &chat_bucket, chat_folder.as_deref(), &active_goals, &combined_ctx);

    // Model routing
    let (default_provider, default_model) = identity::route_model(&req.content);
    let provider_str = req.provider.clone().unwrap_or_else(|| default_provider.to_string());
    let model_str = req.model.clone().unwrap_or_else(|| default_model.to_string());
    let provider = LlmProvider::from_str(&provider_str);

    // Tool definitions — filter out tools whose required keys are missing
    let all_tools = all_tools_v3();
    let luma_key_ok = state.config.luma_api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false);
    let google_ai_key_ok = state.config.google_ai_api_key.as_ref().map(|k| !k.is_empty()).unwrap_or(false);
    let tools: Vec<_> = all_tools.into_iter().filter(|t| {
        let name = t.name.as_str();
        // Skip Luma tools if key is missing/invalid
        if name.starts_with("luma_") && !luma_key_ok { return false; }
        // Skip Google AI tools if key is missing
        if (name == "generate_image" || name == "generate_video" || name == "analyze_image"
            || name == "gemini_research" || name == "gemini_chat") && !google_ai_key_ok { return false; }
        true
    }).collect();
    let anthropic_tools = to_anthropic_tools(&tools);

    let (tx, rx) = mpsc::channel::<StreamEvent>(512);
    let llm = state.llm.clone();
    let db = state.db.clone();
    let brave_key = state.config.brave_api_key.clone();
    let google_ai_key = state.config.google_ai_api_key.clone();
    let google_ai_project = state.config.google_ai_project.clone();
    let luma_api_key = state.config.luma_api_key.clone();
    let model_str_clone = model_str.clone();
    let forge_ref = state.forge.clone();

    tokio::spawn(async move {
        let tool_ctx = ToolContext {
            brave_api_key: brave_key.clone(),
            google_ai_key: google_ai_key.clone(),
            google_ai_project: google_ai_project.clone(),
            luma_api_key: luma_api_key.clone(),
            openai_api_key: state.config.openai_api_key.clone(),
            db: db.clone(),
            session_id,
            user_id,
            chat_bucket: chat_bucket.clone(),
            chat_folder: chat_folder.clone(),
            forge: Some(forge_ref.clone()),
        };

        // Agentic loop — stream, execute tools, loop back with results
        let mut conv_messages = messages.clone();
        let mut full_response = String::new();
        
        // ── INTELLIGENT MODEL ROUTING ────────────────────────────────────────
        // Start with Haiku (cheaper, faster for initial tool calls)
        // Switch to Sonnet when we need final reasoning/writing
        let haiku_model = "claude-haiku-4-5";
        let sonnet_model = "claude-sonnet-4-5";
        let mut current_model = haiku_model.to_string();
        
        tracing::info!("Model routing initialized: starting with {}", current_model);

        'agent: for iteration in 0..120 {
            let iter_tx = tx.clone();
            tracing::info!("Agent iteration {} using model {}", iteration, current_model);
            let _ = tx.send(StreamEvent::Iteration { num: iteration as u32 }).await;

            // Stream this turn — returns text + any tool calls Anthropic wants
            let turn_result = llm.stream_with_tools_and_calls(
                &provider,
                &current_model,  // Use the dynamically selected model
                &system,
                conv_messages.clone(),
                16000,
                &anthropic_tools,
                iter_tx,
            ).await;

            match turn_result {
                Err(e) => {
                    tracing::error!("LLM stream error: {}", e);
                    break 'agent;
                }
                Ok((text, tool_calls)) => {
                    if !text.is_empty() {
                        full_response = text.clone();
                    }

                    if tool_calls.is_empty() {
                        // No tool calls — this is the final reasoning/response turn
                        // We're done with the agentic loop
                        tracing::info!("No tool calls, final response complete with {}", current_model);
                        break 'agent;
                    }

                    // Tool calls present — switch to Haiku for the next tool-execution iteration
                    // (we don't need Sonnet's reasoning power to just process tool results)
                    if current_model != haiku_model {
                        tracing::info!("Switching to {} for tool-execution iteration", haiku_model);
                        current_model = haiku_model.to_string();
                    }

                    // Build the assistant message with tool_use blocks for history
                    let mut assistant_content: Vec<serde_json::Value> = vec![];
                    if !text.is_empty() {
                        assistant_content.push(serde_json::json!({ "type": "text", "text": text }));
                    }

                    // Execute each tool call and collect results
                    let mut tool_result_content: Vec<serde_json::Value> = vec![];

                    for tc in &tool_calls {
                        let tool_id   = &tc.id;
                        let tool_name = &tc.name;
                        let tool_input = &tc.input;

                        assistant_content.push(serde_json::json!({
                            "type": "tool_use",
                            "id": tool_id,
                            "name": tool_name,
                            "input": tool_input,
                        }));

                        tracing::info!("Executing tool: {} ({})", tool_name, tool_id);
                        let start = std::time::Instant::now();

                        let result = crate::tools::execute_tool(tool_name, tool_input, &tool_ctx).await;
                        let duration_ms = result.duration_ms;
                        let success = result.success;
                        let output_val = result.output.clone();
                        let output_str = serde_json::to_string(&output_val).unwrap_or_default();
                        tracing::info!("Tool {} done in {}ms success={}", tool_name, duration_ms, success);

                        // Emit system event on tool failure
                        if !success {
                            if let Some(error_msg) = output_val.get("error").and_then(|e| e.as_str()) {
                                crate::system_events::emit_tool_failure(&db, tool_name, error_msg, None).await;
                            }
                        }

                        // Send tool_result SSE event to UI
                        let _ = tx.send(crate::llm::StreamEvent::ToolResult {
                            id: tool_id.clone(),
                            name: tool_name.clone(),
                            success,
                            output: output_val,
                            duration_ms,
                        }).await;

                        tool_result_content.push(serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": tool_id,
                            "content": output_str,
                        }));
                    }

                    // Add assistant turn (with tool_use blocks) to history
                    conv_messages.push(crate::llm::ChatMessage {
                        role: "assistant".to_string(),
                        content: serde_json::to_string(&assistant_content).unwrap_or_default(),
                    });

                    // Add tool results as user turn
                    conv_messages.push(crate::llm::ChatMessage {
                        role: "user".to_string(),
                        content: serde_json::to_string(&tool_result_content).unwrap_or_default(),
                    });
                    
                    // After processing tool results, check if we should switch to Sonnet
                    // for the next iteration (when we need reasoning on tool outputs)
                    // Heuristic: if we have accumulated text or this is not the first iteration,
                    // switch to Sonnet to process results and potentially give final answer
                    if iteration > 0 && current_model == haiku_model {
                        tracing::info!("Switching to {} for reasoning on tool results", sonnet_model);
                        current_model = sonnet_model.to_string();
                    }
                    
                    // Loop back — let Frank process the tool results
                }
            }
        }

        // ── PERSIST assistant message (the bug fix) ──────────────────────────
        // If no text response was produced (all iterations used for tool calls),
        // store a status message so the session doesn't appear broken/silent.
        if full_response.is_empty() {
            full_response = "I hit my iteration limit on this task — it was more complex than expected. Check FRANK_TO_MAC.md for build progress, or ask me to continue where I left off.".to_string();
            let _ = tx.send(StreamEvent::Delta(full_response.clone())).await;
        }
        if !full_response.is_empty() {
            // Score the assistant response
            let (importance, message_type, is_handoff) = score_message(&full_response);
            
            let _ = sqlx::query(
                "INSERT INTO frankos_messages (session_id, user_id, role, content, importance, message_type, is_handoff) 
                 VALUES ($1, $2, 'assistant', $3, $4, $5, $6)"
            ).bind(session_id).bind(user_id).bind(&full_response)
             .bind(importance).bind(&message_type).bind(is_handoff)
             .execute(&db).await;

            // Auto-promote milestones (importance 9+) to build_state memory
            if importance >= 9 {
                let db_clone = db.clone();
                let title = full_response.chars().take(80).collect::<String>();
                let content = full_response.clone();
                let msg_type = message_type.clone();
                
                tokio::spawn(async move {
                    let _ = memory::store(
                        &db_clone, "build_state", "chuck_frank",
                        &msg_type, &title, &content,
                        importance, &[], None, None, Some(session_id), "auto_milestone"
                    ).await;
                });
            }

            // Fire-and-forget memory extraction
            let db2 = db.clone();
            let llm2 = llm.clone();
            let user_msg = clean_content.clone();
            let assistant_msg = full_response.clone();
            let bucket_clone = chat_bucket.clone();
            let folder_clone = chat_folder.clone();
            tokio::spawn(async move {
                let _ = crate::extractor::maybe_extract_and_store(
                    &db2, &llm2, "chuck_frank", session_id, &user_msg, &assistant_msg,
                    &bucket_clone, folder_clone.as_deref(),
                ).await;
            });
        }

        // Spawn any pending agents (created by spawn_agent tool)
        spawn_pending_agents(&db, llm, brave_key).await;

        // Signal done
        let _ = tx.send(StreamEvent::Done).await;
    });

    // Convert StreamEvent channel to SSE events
    let event_stream = ReceiverStream::new(rx).map(|event| {
        let sse_event = match &event {
            StreamEvent::Delta(text) => {
                // Send text as-is — SSE handles multiline data natively
                Event::default().event("delta").data(text.clone())
            }
            StreamEvent::Iteration { num } => {
                Event::default().event("iteration").data(
                    serde_json::to_string(&json!({ "num": num })).unwrap_or_default()
                )
            }
            StreamEvent::ToolStart { id, name } => {
                Event::default().event("tool_start").data(
                    serde_json::to_string(&json!({ "id": id, "name": name })).unwrap_or_default()
                )
            }
            StreamEvent::ToolInput { id, name, input } => {
                Event::default().event("tool_input").data(
                    serde_json::to_string(&json!({ "id": id, "name": name, "input": input })).unwrap_or_default()
                )
            }
            StreamEvent::ToolResult { id, name, success, output, duration_ms } => {
                Event::default().event("tool_result").data(
                    serde_json::to_string(&json!({
                        "id": id, "name": name, "success": success,
                        "output": output, "duration_ms": duration_ms
                    })).unwrap_or_default()
                )
            }
            StreamEvent::Notification { title, body } => {
                Event::default().event("notification").data(
                    serde_json::to_string(&json!({ "title": title, "body": body })).unwrap_or_default()
                )
            }
            StreamEvent::Done => {
                Event::default().event("done").data("")
            }
        };
        Ok::<Event, std::convert::Infallible>(sse_event)
    });

    Ok(Sse::new(event_stream).keep_alive(KeepAlive::default()))
}
