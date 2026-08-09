//! API route definitions

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{get, post, put},
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::{auth, extractor, identity, llm::{ChatMessage, LlmProvider}, memory, semantic_search, tools, tool_registry, AppState};

pub fn api_router() -> Router<AppState> {
    Router::new()
        .route("/auth/register", post(auth_register))
        .route("/auth/login", post(auth_login))
        .route("/chat/sessions", get(list_sessions).post(create_session))
        .route("/chat/sessions/:id/messages", get(get_messages).post(send_message))
        .route("/identity/:doc_id", get(get_identity_doc).put(put_identity_doc))
        .route("/notifications/test", post(notifications_test))
        .route("/activity/live", get(activity_live))
        .route("/memory/search_semantic", post(memory_search_semantic))
        .route("/memory/search_hybrid", post(memory_search_hybrid))
        .route("/tools/exec", post(tools_exec))
        .route("/tools/pipeline", post(tools_pipeline))
        // Gap 9: Tool Registry
        .route("/tools/registry", get(registry_list_tools).post(registry_register_tool))
        .route("/tools/registry/:tool_id", get(registry_get_tool))
        .route("/tools/registry/:tool_id/health", post(registry_health_check))
        .route("/tools/registry/:tool_id/deprecate", post(registry_deprecate_tool))
        // Gap 10B: Agent Mailbox
        .route("/mailbox/write", post(mailbox_write))
        .route("/mailbox/read", get(mailbox_read))
        .route("/mailbox/mark_read", post(mailbox_mark_read))
        // Gap 11: Agent Spawn
        .route("/agent/spawn", post(agent_spawn_endpoint))
}

fn extract_user(headers: &HeaderMap, secret: &str) -> Option<(Uuid, String, String)> {
    let auth = headers.get("Authorization")?.to_str().ok()?;
    let token = auth.strip_prefix("Bearer ")?;
    let claims = auth::verify_token(token, secret).ok()?;
    let user_id = claims.sub.parse().ok()?;
    Some((user_id, claims.email, claims.role))
}

// ── Auth ──────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RegisterRequest {
    email: String,
    name: String,
    password: String,
}

#[derive(Serialize)]
struct AuthResponse {
    token: String,
    user_id: String,
    email: String,
    name: String,
    role: String,
    relationship: String,
    is_master_user: bool,
}

async fn auth_register(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<AuthResponse>, (StatusCode, Json<Value>)> {
    let hash = bcrypt::hash(&req.password, bcrypt::DEFAULT_COST)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    let user_id: Uuid = sqlx::query_scalar(
        "INSERT INTO frankos_users (email, name, password_hash) VALUES ($1, $2, $3) RETURNING id"
    )
    .bind(&req.email).bind(&req.name).bind(&hash)
    .fetch_one(&state.db).await
    .map_err(|e| (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))))?;

    let token = auth::create_token(&user_id, &req.email, "user", &state.config.jwt_secret)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    Ok(Json(AuthResponse {
        token, user_id: user_id.to_string(), email: req.email, name: req.name,
        role: "user".to_string(), relationship: "user".to_string(), is_master_user: false,
    }))
}

async fn auth_login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<AuthResponse>, (StatusCode, Json<Value>)> {
    let row = sqlx::query(
        "SELECT id, email, name, password_hash, role, relationship, is_master_user FROM frankos_users WHERE email = $1"
    )
    .bind(&req.email)
    .fetch_optional(&state.db).await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?
    .ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(json!({"error": "Invalid credentials"}))))?;

    let hash: Option<String> = row.try_get("password_hash").unwrap_or(None);
    let valid = bcrypt::verify(&req.password, &hash.unwrap_or_default())
        .map_err(|_| (StatusCode::UNAUTHORIZED, Json(json!({"error": "Invalid credentials"}))))?;
    if !valid {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Invalid credentials"}))));
    }

    let user_id: Uuid = row.try_get("id").map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
    let email: String = row.try_get("email").unwrap_or_default();
    let name: String = row.try_get("name").unwrap_or_default();

    let token = auth::create_token(&user_id, &email, "user", &state.config.jwt_secret)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    let relationship: String = row.try_get("relationship").unwrap_or_else(|_| "user".to_string());
    let role_str: String = row.try_get("role").unwrap_or_else(|_| "user".to_string());
    let is_master_user: bool = row.try_get("is_master_user").unwrap_or(false);

    Ok(Json(AuthResponse {
        token, user_id: user_id.to_string(), email, name,
        role: role_str, relationship, is_master_user,
    }))
}

#[derive(Deserialize)]
struct LoginRequest { email: String, password: String }

// ── Chat ──────────────────────────────────────────────────────────────────────

async fn list_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (user_id, _, _) = extract_user(&headers, &state.config.jwt_secret)
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))))?;

    let rows = sqlx::query(
        "SELECT id, created_at FROM frankos_sessions WHERE user_id = $1 AND revoked_at IS NULL ORDER BY created_at DESC LIMIT 50"
    )
    .bind(user_id).fetch_all(&state.db).await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    let sessions: Vec<Value> = rows.iter().map(|r| {
        let id: Uuid = r.try_get("id").unwrap_or_default();
        json!({ "id": id.to_string() })
    }).collect();

    Ok(Json(json!({ "sessions": sessions })))
}

async fn create_session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (user_id, _, _) = extract_user(&headers, &state.config.jwt_secret)
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))))?;

    let session_id: Uuid = sqlx::query_scalar(
        "INSERT INTO frankos_sessions (user_id, token_hash, expires_at) VALUES ($1, $2, NOW() + INTERVAL '30 days') RETURNING id"
    )
    .bind(user_id).bind(Uuid::new_v4().to_string())
    .fetch_one(&state.db).await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    // Emit system event
    crate::system_events::emit_session_state_change(&state.db, session_id, "none", "start").await;

    Ok(Json(json!({ "session_id": session_id.to_string() })))
}

async fn get_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<Uuid>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (user_id, _, _) = extract_user(&headers, &state.config.jwt_secret)
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))))?;

    let rows = sqlx::query(
        "SELECT id, role, content, created_at FROM frankos_messages WHERE session_id = $1 AND user_id = $2 ORDER BY created_at ASC"
    )
    .bind(session_id).bind(user_id)
    .fetch_all(&state.db).await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    let messages: Vec<Value> = rows.iter().map(|r| {
        let id: Uuid = r.try_get("id").unwrap_or_default();
        let role: String = r.try_get("role").unwrap_or_default();
        let content: String = r.try_get("content").unwrap_or_default();
        json!({ "id": id.to_string(), "role": role, "content": content })
    }).collect();

    Ok(Json(json!({ "messages": messages })))
}

#[derive(Deserialize)]
struct SendMessageRequest {
    content: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

async fn send_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<Uuid>,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (user_id, email, role) = extract_user(&headers, &state.config.jwt_secret)
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))))?;

    // Store user message
    sqlx::query(
        "INSERT INTO frankos_messages (session_id, user_id, role, content) VALUES ($1, $2, 'user', $3)"
    )
    .bind(session_id).bind(user_id).bind(&req.content)
    .execute(&state.db).await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    // Load conversation history (last 20 messages)
    let history_rows = sqlx::query(
        "SELECT role, content FROM frankos_messages WHERE session_id = $1 AND user_id = $2 ORDER BY created_at ASC LIMIT 20"
    )
    .bind(session_id).bind(user_id)
    .fetch_all(&state.db).await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    let messages: Vec<ChatMessage> = history_rows.iter().map(|r| ChatMessage {
        role: r.try_get("role").unwrap_or_default(),
        content: r.try_get("content").unwrap_or_default(),
    }).collect();

    // Route to appropriate model
    let (default_provider, default_model) = identity::route_model(&req.content);
    let provider_str = req.provider.as_deref().unwrap_or(default_provider);
    let model_str = req.model.as_deref().unwrap_or(default_model);
    let provider = LlmProvider::from_str(provider_str);

    // Get user name for system prompt
    let user_name_row = sqlx::query("SELECT name FROM frankos_users WHERE id = $1")
        .bind(user_id).fetch_optional(&state.db).await.ok().flatten();
    let user_name = user_name_row
        .and_then(|r| r.try_get::<String, _>("name").ok())
        .unwrap_or_else(|| email.split('@').next().unwrap_or("Friend").to_string());

    // Recall memory context (telos + work + project)
    let recall_ctx = memory::recall(&state.db, "chuck_frank", None, 5).await
        .unwrap_or_default();
    let memory_block = recall_ctx.to_context_block();

    let system = identity::system_prompt(&user_name, &role, &memory_block, "personal", None);

    // Call LLM
    let response = state.llm
        .complete(&provider, model_str, &system, &messages, 4096)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": e.to_string()}))))?;

    // Store assistant response
    let msg_id: Uuid = sqlx::query_scalar(
        "INSERT INTO frankos_messages (session_id, user_id, role, content) VALUES ($1, $2, 'assistant', $3) RETURNING id"
    )
    .bind(session_id).bind(user_id).bind(&response)
    .fetch_one(&state.db).await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    // Fire-and-forget memory extraction
    {
        let db2 = state.db.clone();
        let llm2 = state.llm.clone();
        let user_msg = req.content.clone();
        let assistant_msg = response.clone();
        tokio::spawn(async move {
            let _ = extractor::maybe_extract_and_store(
                &db2, &llm2, "chuck_frank", session_id,
                &user_msg, &assistant_msg, "personal", None,
            ).await;
        });
    }

    Ok(Json(json!({
        "message_id": msg_id.to_string(),
        "role": "assistant",
        "content": response,
        "model": model_str,
        "provider": provider_str,
    })))
}

// Public version for use by other modules (voice, etc.)
pub fn extract_user_pub(headers: &axum::http::HeaderMap, secret: &str) -> Option<(uuid::Uuid, String, String)> {
    extract_user(headers, secret)
}

// ── Identity document endpoints ────────────────────────────────────────────

const VALID_DOC_IDS: &[&str] = &["soul", "ethos", "telos", "constitution", "training"];

async fn get_identity_doc(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some((_user_id, _email, _role)) = extract_user(&headers, &state.config.jwt_secret) else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))));
    };
    if !VALID_DOC_IDS.contains(&doc_id.as_str()) {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "Invalid doc_id"}))));
    }
    let row = sqlx::query(
        "SELECT content FROM frankos_memory WHERE scope = 'identity' AND namespace = $1 LIMIT 1"
    )
    .bind(&doc_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    let content: String = row
        .map(|r| r.try_get::<String, _>("content").unwrap_or_default())
        .unwrap_or_default();

    Ok(Json(json!({ "doc_id": doc_id, "content": content })))
}

#[derive(Deserialize)]
struct IdentityDocRequest {
    content: String,
}

async fn put_identity_doc(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
    Json(req): Json<IdentityDocRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some((_user_id, _email, _role)) = extract_user(&headers, &state.config.jwt_secret) else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))));
    };
    if !VALID_DOC_IDS.contains(&doc_id.as_str()) {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "Invalid doc_id"}))));
    }
    sqlx::query(
        "INSERT INTO frankos_memory (scope, namespace, content, importance, memory_type, title)
         VALUES ('identity', $1, $2, 10, 'identity_doc', $1)
         ON CONFLICT (scope, namespace) WHERE scope = 'identity'
         DO UPDATE SET content = EXCLUDED.content, updated_at = NOW()"
    )
    .bind(&doc_id)
    .bind(&req.content)
    .execute(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    Ok(Json(json!({ "ok": true, "doc_id": doc_id })))
}

// ── Notifications test endpoint ───────────────────────────────────────────────

#[derive(Deserialize)]
struct NotifyTestRequest {
    to: Option<String>,
    subject: Option<String>,
    body: Option<String>,
}

async fn notifications_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<NotifyTestRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some((_user_id, email, _role)) = extract_user(&headers, &state.config.jwt_secret) else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))));
    };

    let to = req.to.unwrap_or(email);
    let subject = req.subject.unwrap_or_else(|| "Frank — Test Notification".to_string());
    let body_text = req.body.unwrap_or_else(|| "If you are reading this, proactive email delivery is working!".to_string());

    let key = state.config.resend_api_key.clone().unwrap_or_default();
    if key.is_empty() {
        return Err((StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "RESEND_API_KEY not configured"}))));
    }

    state.delivery.send_email_raw(&key, &to, &subject, &body_text).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    Ok(Json(json!({ "ok": true, "to": to, "subject": subject })))
}


// ── Live Activity Feed ────────────────────────────────────────────────────────

async fn activity_live(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some((_user_id, _email, _role)) = extract_user(&headers, &state.config.jwt_secret) else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))));
    };

    let rows = sqlx::query(
        "SELECT id, tool_name, input, output, success, created_at, completed_at,          EXTRACT(EPOCH FROM (COALESCE(completed_at, NOW()) - created_at)) * 1000 AS duration_ms          FROM frankos_agent_tool_calls ORDER BY created_at DESC LIMIT 50"
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    let events: Vec<Value> = rows.iter().map(|r| {
        let created_at: chrono::DateTime<chrono::Utc> = r.try_get("created_at").unwrap_or_default();
        let completed_at: Option<chrono::DateTime<chrono::Utc>> = r.try_get("completed_at").ok().flatten();
        let duration_ms: f64 = r.try_get("duration_ms").unwrap_or(0.0);
        let success: bool = r.try_get("success").unwrap_or(false);
        let id: uuid::Uuid = r.try_get("id").unwrap_or_default();
        let tool_name: String = r.try_get("tool_name").unwrap_or_default();
        let input: Value = r.try_get("input").unwrap_or(Value::Null);
        let output: Value = r.try_get("output").unwrap_or(Value::Null);
        let status = if completed_at.is_some() { if success { "done" } else { "error" } } else { "running" };
        json!({
            "id": id.to_string(),
            "tool": tool_name,
            "input": input,
            "output": output,
            "success": success,
            "status": status,
            "duration_ms": duration_ms as i64,
            "created_at": created_at.to_rfc3339(),
        })
    }).collect();

    let is_active = events.iter().any(|e| e["status"] == "running");
    let last_event_at = events.first().map(|e| e["created_at"].as_str().unwrap_or("").to_string());

    Ok(Json(json!({
        "is_active": is_active,
        "last_event_at": last_event_at,
        "events": events,
        "count": events.len(),
    })))
}


// ── Semantic Memory Search ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SemanticSearchRequest {
    query: String,
    #[serde(default = "default_limit")]
    limit: i32,
    #[serde(default)]
    threshold: Option<f32>,
    #[serde(default = "default_namespace")]
    namespace: String,
}

fn default_limit() -> i32 { 10 }
fn default_namespace() -> String { "chuck_frank".to_string() }

async fn memory_search_semantic(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SemanticSearchRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some((_user_id, _email, _role)) = extract_user(&headers, &state.config.jwt_secret) else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))));
    };

    let api_key = state.config.openai_api_key.clone().unwrap_or_default();
    if api_key.is_empty() {
        return Err((StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "OPENAI_API_KEY not configured"}))));
    }

    let results = semantic_search::semantic_search(
        &state.db,
        &req.query,
        &req.namespace,
        &api_key,
        req.limit,
        req.threshold,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    let items: Vec<Value> = results.iter().map(|r| json!({
        "id": r.id,
        "title": r.title,
        "content": r.content,
        "memory_type": r.memory_type,
        "namespace": r.namespace,
        "bucket": r.bucket,
        "importance": r.importance,
        "tags": r.tags,
        "similarity": r.similarity,
        "created_at": r.created_at.to_rfc3339(),
    })).collect();

    Ok(Json(json!({
        "query": req.query,
        "count": items.len(),
        "results": items,
    })))
}

async fn memory_search_hybrid(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SemanticSearchRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some((_user_id, _email, _role)) = extract_user(&headers, &state.config.jwt_secret) else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))));
    };

    let api_key = state.config.openai_api_key.clone().unwrap_or_default();
    if api_key.is_empty() {
        return Err((StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "OPENAI_API_KEY not configured"}))));
    }

    let results = semantic_search::hybrid_search(
        &state.db,
        &req.query,
        &req.namespace,
        &api_key,
        req.limit,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    let items: Vec<Value> = results.iter().map(|r| json!({
        "id": r.id,
        "title": r.title,
        "content": r.content,
        "memory_type": r.memory_type,
        "namespace": r.namespace,
        "bucket": r.bucket,
        "importance": r.importance,
        "tags": r.tags,
        "similarity": r.similarity,
        "created_at": r.created_at.to_rfc3339(),
    })).collect();

    Ok(Json(json!({
        "query": req.query,
        "count": items.len(),
        "results": items,
    })))
}

// ── Direct Tool Execution (Gap 6) ─────────────────────────────────────────────

#[derive(Deserialize)]
struct ToolExecRequest {
    tool: String,
    input: Value,
}

async fn tools_exec(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ToolExecRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (user_id, _, _) = extract_user(&headers, &state.config.jwt_secret)
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))))?;

    // Build minimal ToolContext for direct execution — no LLM involved
    let ctx = tools::ToolContext {
        brave_api_key: state.config.brave_api_key.clone(),
        google_ai_key: state.config.google_ai_api_key.clone(),
        google_ai_project: state.config.google_ai_project.clone(),
        luma_api_key: state.config.luma_api_key.clone(),
        openai_api_key: state.config.openai_api_key.clone(),
        db: state.db.clone(),
        session_id: Uuid::new_v4(),
        user_id,
        chat_bucket: "direct_tool_exec".to_string(),
        chat_folder: None,
        forge: Some(state.forge.clone()),
    };

    let result = tools::execute_tool(&req.tool, &req.input, &ctx).await;

    Ok(Json(json!({
        "success": result.success,
        "tool": result.tool_name,
        "output": result.output,
        "duration_ms": result.duration_ms,
    })))
}

// ── Tool Pipeline (Gap 6C) — sequential execution ─────────────────────────────

#[derive(Deserialize)]
struct ToolPipelineRequest {
    tools: Vec<ToolPipelineStep>,
}

#[derive(Deserialize)]
struct ToolPipelineStep {
    tool: String,
    input: Value,
}

async fn tools_pipeline(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ToolPipelineRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (user_id, _, _) = extract_user(&headers, &state.config.jwt_secret)
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))))?;

    let ctx = tools::ToolContext {
        brave_api_key: state.config.brave_api_key.clone(),
        google_ai_key: state.config.google_ai_api_key.clone(),
        google_ai_project: state.config.google_ai_project.clone(),
        luma_api_key: state.config.luma_api_key.clone(),
        openai_api_key: state.config.openai_api_key.clone(),
        db: state.db.clone(),
        session_id: Uuid::new_v4(),
        user_id,
        chat_bucket: "tool_pipeline".to_string(),
        chat_folder: None,
        forge: Some(state.forge.clone()),
    };

    let mut results = Vec::new();
    let mut context_map: std::collections::HashMap<String, Value> = std::collections::HashMap::new();

    for (idx, step) in req.tools.iter().enumerate() {
        // Allow input templating: if input contains {"$ref": "step_N"}, substitute previous output
        let resolved_input = resolve_refs(&step.input, &context_map);

        let result = tools::execute_tool(&step.tool, &resolved_input, &ctx).await;

        // Store this result for future $ref substitutions
        context_map.insert(format!("step_{}", idx), result.output.clone());

        results.push(json!({
            "step": idx,
            "tool": result.tool_name,
            "success": result.success,
            "output": result.output,
            "duration_ms": result.duration_ms,
        }));

        // Stop pipeline on first failure
        if !result.success {
            break;
        }
    }

    Ok(Json(json!({
        "success": results.iter().all(|r| r["success"].as_bool().unwrap_or(false)),
        "steps": results,
    })))
}

// Helper: resolve {"$ref": "step_N"} references in input
fn resolve_refs(input: &Value, context: &std::collections::HashMap<String, Value>) -> Value {
    match input {
        Value::Object(map) => {
            if let Some(ref_key) = map.get("$ref") {
                if let Some(ref_str) = ref_key.as_str() {
                    return context.get(ref_str).cloned().unwrap_or(input.clone());
                }
            }
            Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), resolve_refs(v, context)))
                    .collect(),
            )
        }
        Value::Array(arr) => Value::Array(arr.iter().map(|v| resolve_refs(v, context)).collect()),
        _ => input.clone(),
    }
}

// ── Tool Registry Endpoints (Gap 9) ───────────────────────────────────────────

#[derive(Deserialize)]
struct RegisterToolRequest {
    name: String,
    version: String,
    spec: tool_registry::ToolSpec,
    certified_by: String,
}

async fn registry_register_tool(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RegisterToolRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some((_user_id, _email, _role)) = extract_user(&headers, &state.config.jwt_secret) else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))));
    };

    let entry = tool_registry::register_tool(
        &state.db,
        req.name,
        req.version,
        req.spec,
        req.certified_by,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    Ok(Json(json!({
        "tool_id": entry.tool_id,
        "name": entry.name,
        "version": entry.version,
        "status": entry.status,
        "certified_at": entry.certified_at.to_rfc3339(),
    })))
}

async fn registry_list_tools(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some((_user_id, _email, _role)) = extract_user(&headers, &state.config.jwt_secret) else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))));
    };

    let tools = tool_registry::list_tools(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    let items: Vec<Value> = tools.iter().map(|t| json!({
        "tool_id": t.tool_id,
        "name": t.name,
        "version": t.version,
        "status": t.status,
        "health_status": t.health_status,
        "certified_by": t.certified_by,
        "certified_at": t.certified_at.to_rfc3339(),
        "last_health_check": t.last_health_check.map(|dt| dt.to_rfc3339()),
    })).collect();

    Ok(Json(json!({
        "count": items.len(),
        "tools": items,
    })))
}

async fn registry_get_tool(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tool_id): Path<Uuid>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some((_user_id, _email, _role)) = extract_user(&headers, &state.config.jwt_secret) else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))));
    };

    let tool = tool_registry::get_tool(&state.db, tool_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, Json(json!({"error": "Tool not found"}))))?;

    Ok(Json(json!({
        "tool_id": tool.tool_id,
        "name": tool.name,
        "version": tool.version,
        "spec": serde_json::to_value(&tool.spec).unwrap_or(Value::Null),
        "status": tool.status,
        "health_status": tool.health_status,
        "certified_by": tool.certified_by,
        "certified_at": tool.certified_at.to_rfc3339(),
        "last_health_check": tool.last_health_check.map(|dt| dt.to_rfc3339()),
    })))
}

#[derive(Deserialize)]
struct HealthCheckRequest {
    message: Option<String>,
}

async fn registry_health_check(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tool_id): Path<Uuid>,
    Json(req): Json<HealthCheckRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some((_user_id, _email, _role)) = extract_user(&headers, &state.config.jwt_secret) else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))));
    };

    // For now, stub health check — Gap 10 will implement real checks
    let health_result = tool_registry::check_tool_health(tool_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    let updated = tool_registry::update_health_status(
        &state.db,
        tool_id,
        health_result.status.clone(),
        req.message.or(Some(health_result.message.clone())),
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    Ok(Json(json!({
        "tool_id": updated.tool_id,
        "status": updated.status,
        "message": updated.message,
        "checked_at": updated.checked_at.to_rfc3339(),
    })))
}

async fn registry_deprecate_tool(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tool_id): Path<Uuid>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some((_user_id, _email, _role)) = extract_user(&headers, &state.config.jwt_secret) else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))));
    };

    tool_registry::deprecate_tool(&state.db, tool_id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    Ok(Json(json!({
        "ok": true,
        "tool_id": tool_id,
        "status": "deprecated",
    })))
}


// ── Gap 10B: Agent Mailbox HTTP Endpoints ────────────────────────────────────

#[derive(Deserialize)]
struct MailboxWriteReq {
    to_agent_id: Option<String>,
    message_type: String,
    subject: String,
    content: String,
}

async fn mailbox_write(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<MailboxWriteReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some((user_id, _email, _role)) = extract_user(&headers, &state.config.jwt_secret) else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))));
    };
    let to_id: Option<Uuid> = body.to_agent_id.and_then(|s| Uuid::parse_str(&s).ok());
    let mid = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO frank_agent_mailbox (id, from_agent_id, to_agent_id, message_type, subject, content, status, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, 'unread', NOW())"
    )
    .bind(mid).bind(user_id).bind(to_id)
    .bind(&body.message_type).bind(&body.subject).bind(&body.content)
    .execute(&state.db).await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
    Ok(Json(json!({"success": true, "mailbox_id": mid, "message_type": body.message_type})))
}

async fn mailbox_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some((_uid, _email, _role)) = extract_user(&headers, &state.config.jwt_secret) else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))));
    };
    let agent_id: Option<Uuid> = params.get("agent_id").and_then(|s| Uuid::parse_str(s).ok());
    let status = params.get("status").map(|s| s.as_str().to_string()).unwrap_or_else(|| "unread".to_string());
    let limit: i32 = params.get("limit").and_then(|s| s.parse().ok()).unwrap_or(20);
    let rows: Vec<(Uuid, Option<Uuid>, String, String, String, chrono::DateTime<chrono::Utc>)> = if let Some(aid) = agent_id {
        sqlx::query_as(
            "SELECT id, from_agent_id, message_type, subject, content, created_at
             FROM frank_agent_mailbox WHERE to_agent_id = $1 AND status = $2
             ORDER BY created_at ASC LIMIT $3"
        ).bind(aid).bind(&status).bind(limit).fetch_all(&state.db).await
    } else {
        sqlx::query_as(
            "SELECT id, from_agent_id, message_type, subject, content, created_at
             FROM frank_agent_mailbox WHERE to_agent_id IS NULL AND status = $1
             ORDER BY created_at ASC LIMIT $2"
        ).bind(&status).bind(limit).fetch_all(&state.db).await
    }.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
    let messages: Vec<Value> = rows.iter().map(|(id, from_id, mt, subj, cont, created)| {
        json!({"id": id, "from_agent_id": from_id, "message_type": mt, "subject": subj, "content": cont, "created_at": created.to_rfc3339()})
    }).collect();
    Ok(Json(json!({"success": true, "messages": messages, "count": messages.len()})))
}

#[derive(Deserialize)]
struct MailboxMarkReadReq {
    mailbox_ids: Vec<String>,
}

async fn mailbox_mark_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<MailboxMarkReadReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some((_uid, _email, _role)) = extract_user(&headers, &state.config.jwt_secret) else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))));
    };
    let uuids: Vec<Uuid> = body.mailbox_ids.iter().filter_map(|s| Uuid::parse_str(s).ok()).collect();
    if uuids.is_empty() {
        return Ok(Json(json!({"success": false, "error": "No valid UUIDs"})));
    }
    let result = sqlx::query("UPDATE frank_agent_mailbox SET status = 'read' WHERE id = ANY($1)")
        .bind(&uuids).execute(&state.db).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
    Ok(Json(json!({"success": true, "updated": result.rows_affected()})))
}

// ── Gap 11: Agent Spawn HTTP Endpoint ────────────────────────────────────────

#[derive(Deserialize)]
struct AgentSpawnReq {
    name: String,
    goal: String,
    model: Option<String>,
    context: Option<String>,
}

async fn agent_spawn_endpoint(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AgentSpawnReq>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some((user_id, _email, _role)) = extract_user(&headers, &state.config.jwt_secret) else {
        return Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))));
    };

    let name = &body.name;
    let goal = &body.goal;
    let model = body.model.as_deref().unwrap_or("claude-sonnet-4-5");
    let mut context = body.context.clone().unwrap_or_default();
    
    // Check if this matches a persistent agent
    let persistent: Option<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id, system_prompt, memory_ns FROM frank_persistent_agents WHERE name = $1 AND status != 'archived'"
    ).bind(name).fetch_optional(&state.db).await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
    
    let agent_id = Uuid::new_v4();
    
    if let Some((persistent_id, system_prompt, memory_ns)) = persistent {
        // This is a persistent agent — use its system prompt
        if context.is_empty() {
            context = system_prompt.clone();
        } else {
            context = format!("{}\n\n---\n\nAdditional context for this task:\n{}", system_prompt, context);
        }
        
        // Insert into frankos_agents (ephemeral spawn record)
        sqlx::query(
            "INSERT INTO frankos_agents (id, name, goal, model, parent_session_id, user_id, status, tools_allowed)
             VALUES ($1, $2, $3, $4, $5, $6, 'spawned', '[]')"
        )
        .bind(agent_id)
        .bind(name)
        .bind(goal)
        .bind(model)
        .bind(Uuid::nil()) // No session context for HTTP spawns
        .bind(user_id)
        .execute(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
        
        // Log initial context to persistent conversation history
        sqlx::query(
            "INSERT INTO frank_agent_conversations (agent_id, role, content) VALUES ($1, 'system', $2)"
        ).bind(persistent_id).bind(&context).execute(&state.db).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
        
        // Log the goal as a user message
        sqlx::query(
            "INSERT INTO frank_agent_conversations (agent_id, role, content) VALUES ($1, 'user', $2)"
        ).bind(persistent_id).bind(goal).execute(&state.db).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
        
        Ok(Json(json!({
            "success": true,
            "agent_id": agent_id,
            "persistent_agent_id": persistent_id,
            "name": name,
            "goal": goal,
            "memory_namespace": memory_ns
        })))
    } else {
        // Ephemeral agent
        sqlx::query(
            "INSERT INTO frankos_agents (id, name, goal, model, parent_session_id, user_id, status, tools_allowed)
             VALUES ($1, $2, $3, $4, $5, $6, 'spawned', '[]')"
        )
        .bind(agent_id)
        .bind(name)
        .bind(goal)
        .bind(model)
        .bind(Uuid::nil())
        .bind(user_id)
        .execute(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
        
        Ok(Json(json!({
            "success": true,
            "agent_id": agent_id,
            "name": name,
            "goal": goal
        })))
    }
}
