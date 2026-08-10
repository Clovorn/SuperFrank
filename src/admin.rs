//! Admin API routes — keys, tools, agents, health, system stats

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::{delete, get, post},
    Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::process::Stdio;
use tokio::process::Command;
use uuid::Uuid;

use crate::{routes::extract_user_pub, AppState};

pub fn admin_router() -> Router<AppState> {
    Router::new()
        // API Keys
        .route("/admin/keys",          get(list_keys).post(add_key))
        .route("/admin/keys/detect",    post(detect_key))
        .route("/admin/keys/:id",      delete(delete_key).put(update_key))
        .route("/admin/restart",         post(restart_gateway))
        // Tools
        .route("/admin/tools",         get(list_tools))
        .route("/admin/tools/:name/toggle", post(toggle_tool))
        // Agents
        .route("/admin/agents",        get(list_agents))
        .route("/admin/agents/:id",    get(get_agent))
        // Tasks
        .route("/admin/tasks",            get(crate::tasks::list_tasks).post(crate::tasks::create_task))
        .route("/admin/tasks/:id",        get(crate::tasks::get_task).patch(crate::tasks::update_task))
        .route("/admin/tasks/stream",      get(crate::task_stream::task_stream_handler))
        // Health
        .route("/admin/health",        get(health_check))
        // Usage stats
        .route("/admin/stats",         get(usage_stats))
}

fn require_master(headers: &HeaderMap, secret: &str) -> Result<Uuid, (StatusCode, Json<Value>)> {
    match extract_user_pub(headers, secret) {
        Some((uid, _, _)) => Ok(uid),
        None => Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"})))),
    }
}

// ── API Keys ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AddKeyRequest {
    service: String,
    label: String,
    value: String,
    notes: Option<String>,
}

async fn list_keys(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_master(&headers, &state.config.jwt_secret)?;

    // Ensure table exists
    let _ = sqlx::query(
        "CREATE TABLE IF NOT EXISTS frankos_api_keys (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            service TEXT NOT NULL,
            label TEXT NOT NULL,
            value_masked TEXT NOT NULL,
            notes TEXT,
            use_count INTEGER NOT NULL DEFAULT 0,
            last_used_at TIMESTAMPTZ,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )"
    ).execute(&state.db).await;

    let rows = sqlx::query(
        "SELECT id, service, label, value_masked, notes, use_count, last_used_at, created_at
         FROM frankos_api_keys ORDER BY service, label"
    ).fetch_all(&state.db).await.unwrap_or_default();

    let keys: Vec<Value> = rows.iter().map(|r| {
        let id: Uuid = r.try_get("id").unwrap_or_default();
        let last_used: Option<chrono::DateTime<chrono::Utc>> = r.try_get("last_used_at").ok().flatten();
        json!({
            "id": id.to_string(),
            "service": r.try_get::<String,_>("service").unwrap_or_default(),
            "label": r.try_get::<String,_>("label").unwrap_or_default(),
            "value_masked": r.try_get::<String,_>("value_masked").unwrap_or_default(),
            "notes": r.try_get::<Option<String>,_>("notes").unwrap_or(None),
            "use_count": r.try_get::<i32,_>("use_count").unwrap_or(0),
            "last_used_at": last_used.map(|t| t.to_rfc3339()),
        })
    }).collect();

    // Also surface known env keys as auto-detected entries
    let env_keys = detect_env_keys(&state);
    Ok(Json(json!({ "keys": keys, "env_keys": env_keys })))
}

fn detect_env_keys(state: &AppState) -> Vec<Value> {
    let mut detected = vec![];

    let active_anthropic  = state.config.anthropic_api_key.is_some();
    let active_openai     = state.config.openai_api_key.is_some();
    let active_brave      = state.config.brave_api_key.is_some();
    let active_google_ai  = state.config.google_ai_api_key.is_some();
    let active_luma       = state.config.luma_api_key.is_some();
    let active_resend     = state.config.resend_api_key.is_some();
    let active_google_oauth = state.config.google_client_secret.is_some();

    let entries: &[(&str, &str, bool)] = &[
        ("anthropic",    "Claude (Anthropic)",  active_anthropic),
        ("openai",       "OpenAI",              active_openai),
        ("brave",        "Brave Search",        active_brave),
        ("google_ai",    "Google AI",           active_google_ai),
        ("lumalabs",     "LumaLabs",            active_luma),
        ("resend",       "Resend Email",        active_resend),
        ("google_oauth", "Google OAuth",        active_google_oauth),
        ("elevenlabs",   "ElevenLabs TTS",      false),
        ("stripe",       "Stripe Billing",      false),
    ];

    for (svc, label, active) in entries {
        let status = if *active { "active" } else { "not_set" };
        detected.push(json!({ "service": svc, "label": label, "status": status, "source": "env" }));
    }

    detected
}

async fn add_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AddKeyRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_master(&headers, &state.config.jwt_secret)?;

    // Mask value: show first 6 + last 4 chars
    let masked = mask_key(&req.value);

    let _ = sqlx::query(
        "CREATE TABLE IF NOT EXISTS frankos_api_keys (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            service TEXT NOT NULL, label TEXT NOT NULL,
            value_masked TEXT NOT NULL, notes TEXT,
            use_count INTEGER NOT NULL DEFAULT 0,
            last_used_at TIMESTAMPTZ, created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )"
    ).execute(&state.db).await;

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO frankos_api_keys (service, label, value_masked, notes) VALUES ($1,$2,$3,$4) RETURNING id"
    )
    .bind(&req.service).bind(&req.label).bind(&masked).bind(&req.notes)
    .fetch_one(&state.db).await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    // Also write to .env file so gateway can use it
    let env_key = req.service.to_uppercase().replace('-', "_") + "_API_KEY";
    let env_line = format!("\n{}={}", env_key, req.value);
    let env_path = "/opt/frankos/runtime/frankos-gateway/.env";
    if let Ok(mut content) = tokio::fs::read_to_string(env_path).await {
        // Remove old line for this key if present
        let lines: Vec<&str> = content.lines().filter(|l| !l.starts_with(&env_key)).collect();
        content = lines.join("\n") + &env_line;
        let _ = tokio::fs::write(env_path, content).await;
    }

    Ok(Json(json!({ "id": id.to_string(), "service": req.service, "label": req.label, "masked": masked })))
}

async fn delete_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_master(&headers, &state.config.jwt_secret)?;
    sqlx::query("DELETE FROM frankos_api_keys WHERE id = $1")
        .bind(id).execute(&state.db).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;
    Ok(Json(json!({ "deleted": true })))
}

fn mask_key(key: &str) -> String {
    if key.len() <= 10 { return "•".repeat(key.len()); }
    format!("{}•••••{}", &key[..6], &key[key.len()-4..])
}

// ── Tools ─────────────────────────────────────────────────────────────────────

async fn list_tools(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_master(&headers, &state.config.jwt_secret)?;

    // Get tool call stats from agent_tool_calls
    let stats_rows = sqlx::query(
        "SELECT tool_name,
                COUNT(*) as total_calls,
                SUM(CASE WHEN success THEN 1 ELSE 0 END) as success_count,
                AVG(EXTRACT(EPOCH FROM (completed_at - created_at)) * 1000) as avg_ms
         FROM frankos_agent_tool_calls
         WHERE created_at > NOW() - INTERVAL '24 hours'
         GROUP BY tool_name"
    ).fetch_all(&state.db).await.unwrap_or_default();

    let mut stats: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    for row in &stats_rows {
        let name: String = row.try_get("tool_name").unwrap_or_default();
        let total: i64 = row.try_get("total_calls").unwrap_or(0);
        let success: i64 = row.try_get("success_count").unwrap_or(0);
        let avg_ms: f64 = row.try_get("avg_ms").unwrap_or(0.0);
        stats.insert(name, json!({ "calls_24h": total, "success": success, "avg_ms": avg_ms as i64 }));
    }

    // Build tool list from registry
    let tools: Vec<Value> = crate::tools::all_tools().iter().map(|t| {
        let stat = stats.get(&t.name).cloned().unwrap_or(json!({ "calls_24h": 0, "success": 0, "avg_ms": 0 }));
        let required_key = match t.name.as_str() {
            "web_search" => Some("brave"),
            "web_fetch"  => None,
            _ => None,
        };
        let key_status = required_key.map(|k| match k {
            "brave" => if state.config.brave_api_key.is_some() { "active" } else { "missing" },
            _ => "active",
        });
        json!({
            "name": t.name,
            "description": t.description,
            "enabled": true,
            "required_key": required_key,
            "key_status": key_status,
            "stats": stat,
        })
    }).collect();

    Ok(Json(json!({ "tools": tools })))
}

async fn toggle_tool(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_master(&headers, &state.config.jwt_secret)?;
    // For now just acknowledge — full enable/disable registry coming later
    Ok(Json(json!({ "tool": name, "toggled": true })))
}

// ── Agents ────────────────────────────────────────────────────────────────────

async fn list_agents(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_master(&headers, &state.config.jwt_secret)?;

    let rows = sqlx::query(
        "SELECT id, name, goal, status, model, iterations, result, error,
                created_at, started_at, completed_at
         FROM frankos_agents
         ORDER BY created_at DESC LIMIT 50"
    ).fetch_all(&state.db).await.unwrap_or_default();

    let agents: Vec<Value> = rows.iter().map(|r| {
        let id: Uuid = r.try_get("id").unwrap_or_default();
        json!({
            "id": id.to_string(),
            "name": r.try_get::<String,_>("name").unwrap_or_default(),
            "goal": r.try_get::<String,_>("goal").unwrap_or_default(),
            "status": r.try_get::<String,_>("status").unwrap_or_default(),
            "model": r.try_get::<String,_>("model").unwrap_or_default(),
            "iterations": r.try_get::<i32,_>("iterations").unwrap_or(0),
            "result": r.try_get::<Option<String>,_>("result").unwrap_or(None),
            "error": r.try_get::<Option<String>,_>("error").unwrap_or(None),
        })
    }).collect();

    Ok(Json(json!({ "agents": agents })))
}

async fn get_agent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_master(&headers, &state.config.jwt_secret)?;

    let row = sqlx::query(
        "SELECT id, name, goal, status, model, iterations, result, error,
                tools_allowed, created_at, started_at, completed_at
         FROM frankos_agents WHERE id = $1"
    ).bind(id).fetch_optional(&state.db).await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?
    .ok_or_else(|| (StatusCode::NOT_FOUND, Json(json!({"error": "Agent not found"}))))?;

    // Get tool calls for this agent
    let tool_calls = sqlx::query(
        "SELECT tool_name, input, output, success, created_at, completed_at
         FROM frankos_agent_tool_calls WHERE agent_id = $1 ORDER BY created_at ASC"
    ).bind(id).fetch_all(&state.db).await.unwrap_or_default();

    let calls: Vec<Value> = tool_calls.iter().map(|r| json!({
        "tool_name": r.try_get::<String,_>("tool_name").unwrap_or_default(),
        "input": r.try_get::<Value,_>("input").unwrap_or(json!({})),
        "output": r.try_get::<Value,_>("output").unwrap_or(json!({})),
        "success": r.try_get::<Option<bool>,_>("success").unwrap_or(None),
    })).collect();

    let agent_id: Uuid = row.try_get("id").unwrap_or_default();
    Ok(Json(json!({
        "id": agent_id.to_string(),
        "name": row.try_get::<String,_>("name").unwrap_or_default(),
        "goal": row.try_get::<String,_>("goal").unwrap_or_default(),
        "status": row.try_get::<String,_>("status").unwrap_or_default(),
        "model": row.try_get::<String,_>("model").unwrap_or_default(),
        "iterations": row.try_get::<i32,_>("iterations").unwrap_or(0),
        "result": row.try_get::<Option<String>,_>("result").unwrap_or(None),
        "error": row.try_get::<Option<String>,_>("error").unwrap_or(None),
        "tool_calls": calls,
    })))
}

// ── Health ────────────────────────────────────────────────────────────────────

async fn health_check(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_master(&headers, &state.config.jwt_secret)?;

    // DB check
    let db_ok = sqlx::query("SELECT 1").execute(&state.db).await.is_ok();

    // System metrics via shell
    let mem = shell_one("free -m | awk 'NR==2{printf \"%s/%s\", $3, $2}'").await;
    let disk = shell_one("df -h /opt/frankos | awk 'NR==2{print $3\"/\"$2\" (\"$5\")\"}'").await;
    let cpu = shell_one("top -bn1 | grep 'Cpu(s)' | awk '{print $2}' | cut -d'%' -f1").await;
    let uptime = shell_one("systemctl show frankos-gateway --property=ActiveEnterTimestamp --value").await;
    let load = shell_one("cat /proc/loadavg | awk '{print $1, $2, $3}'").await;

    // Message count
    let msg_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM frankos_messages")
        .fetch_one(&state.db).await.unwrap_or(0);

    let agent_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM frankos_agents WHERE status = 'running'")
        .fetch_one(&state.db).await.unwrap_or(0);

    Ok(Json(json!({
        "gateway": {
            "status": "running",
            "version": crate::identity::FRANK_VERSION,
        },
        "database": {
            "status": if db_ok { "connected" } else { "error" },
            "message_count": msg_count,
        },
        "system": {
            "memory": mem,
            "disk": disk,
            "cpu_percent": cpu,
            "load_avg": load,
            "uptime_since": uptime,
        },
        "agents": {
            "running": agent_count,
        },
        "llm": {
            "anthropic": state.config.anthropic_api_key.is_some(),
            "openai": state.config.openai_api_key.is_some(),
            "brave": state.config.brave_api_key.is_some(),
        }
    })))
}

async fn shell_one(cmd: &str) -> String {
    let out = Command::new("bash").arg("-c").arg(cmd)
        .stdout(Stdio::piped()).stderr(Stdio::null())
        .output().await;
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => "unknown".to_string(),
    }
}

// ── Usage Stats ───────────────────────────────────────────────────────────────

async fn usage_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_master(&headers, &state.config.jwt_secret)?;

    let today_msgs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM frankos_messages WHERE created_at > NOW() - INTERVAL '24 hours'"
    ).fetch_one(&state.db).await.unwrap_or(0);

    let month_msgs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM frankos_messages WHERE created_at > NOW() - INTERVAL '30 days'"
    ).fetch_one(&state.db).await.unwrap_or(0);

    let today_tools: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM frankos_agent_tool_calls WHERE created_at > NOW() - INTERVAL '24 hours'"
    ).fetch_one(&state.db).await.unwrap_or(0);

    let today_agents: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM frankos_agents WHERE created_at > NOW() - INTERVAL '24 hours'"
    ).fetch_one(&state.db).await.unwrap_or(0);

    let memory_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM frankos_memory"
    ).fetch_one(&state.db).await.unwrap_or(0);

    Ok(Json(json!({
        "today": {
            "messages": today_msgs,
            "tool_calls": today_tools,
            "agents_spawned": today_agents,
        },
        "month": {
            "messages": month_msgs,
        },
        "total": {
            "memories": memory_count,
        }
    })))
}

// ── Update / Rotate Key ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct UpdateKeyRequest {
    value: String,
    label: Option<String>,
    notes: Option<String>,
}

async fn update_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateKeyRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_master(&headers, &state.config.jwt_secret)?;

    // Fetch existing record to get service name
    let row = sqlx::query("SELECT service, label FROM frankos_api_keys WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, Json(json!({"error": "Key not found"}))))?;

    let service: String = row.try_get("service").unwrap_or_default();
    let existing_label: String = row.try_get("label").unwrap_or_default();
    let new_label = req.label.as_deref().unwrap_or(&existing_label);
    let masked = mask_key(&req.value);

    // Update DB record
    sqlx::query(
        "UPDATE frankos_api_keys SET value_masked = $1, label = $2, notes = $3 WHERE id = $4"
    )
    .bind(&masked)
    .bind(new_label)
    .bind(&req.notes)
    .bind(id)
    .execute(&state.db).await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    // Update .env file — replace existing line for this service
    let env_key = service.to_uppercase().replace('-', "_") + "_API_KEY";
    let env_path = "/opt/frankos/runtime/frankos-gateway/.env";
    if let Ok(content) = tokio::fs::read_to_string(env_path).await {
        let lines: Vec<&str> = content.lines()
            .filter(|l| !l.starts_with(&env_key) && !l.starts_with(&format!("# {}", env_key)))
            .collect();
        let new_content = lines.join("\n") + &format!("\n{}={}", env_key, req.value);
        let _ = tokio::fs::write(env_path, new_content).await;
    }

    Ok(Json(json!({
        "updated": true,
        "id": id.to_string(),
        "service": service,
        "masked": masked,
        "note": "Key updated. Restart gateway to apply."
    })))
}

// ── Restart Gateway ───────────────────────────────────────────────────────────

async fn restart_gateway(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_master(&headers, &state.config.jwt_secret)?;

    tracing::info!("Gateway restart requested via admin API");

    // Spawn restart in background — response goes out before process exits
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let _ = Command::new("systemctl")
            .args(["restart", "frankos-gateway.service"])
            .output().await;
    });

    Ok(Json(json!({
        "restarting": true,
        "message": "Gateway restart initiated. Reconnect in ~3 seconds."
    })))
}

// ── Service catalog — auto-detect service from key prefix ────────────────────

pub fn detect_service(key: &str) -> Option<(&'static str, &'static str)> {
    // Returns (service_id, human_label)
    let key = key.trim();
    if key.starts_with("sk-ant-")           { return Some(("anthropic",   "Anthropic Claude")); }
    if key.starts_with("sk-")               { return Some(("openai",      "OpenAI GPT")); }
    if key.starts_with("BSA")               { return Some(("brave",       "Brave Search")); }
    if key.starts_with("sk_live_") || key.starts_with("sk_test_") {
                                              return Some(("stripe",      "Stripe Billing")); }
    if key.starts_with("re_")               { return Some(("resend",      "Resend Email")); }
    if key.len() == 32 && key.chars().all(|c| c.is_ascii_hexdigit()) {
                                              return Some(("elevenlabs",  "ElevenLabs TTS")); }
    None
}

async fn detect_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    require_master(&headers, &state.config.jwt_secret)?;
    let key = body["value"].as_str().unwrap_or("");
    if let Some((service, label)) = detect_service(key) {
        Ok(Json(json!({ "detected": true, "service": service, "label": label })))
    } else {
        Ok(Json(json!({ "detected": false })))
    }
}

/// GET /api/v1/engineer/status — current Engineer resident loop state
pub async fn engineer_status_handler(
    State(state): State<AppState>,
) -> Json<Value> {
    let status = state.engineer_status.read().await;
    Json(serde_json::to_value(&*status).unwrap_or(serde_json::json!({"error": "serialize failed"})))
}
