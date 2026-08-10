//! Gap 8A — System Events Instrumentation
//! Structured event log for memory writes, searches, tool panics, deploy events, etc.
//! Schema: (id, event_type, severity, payload, created_at)

use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

/// Event type constants
pub mod event_type {
    pub const MEMORY_WRITE: &str = "memory_write";
    pub const MEMORY_SEARCH: &str = "memory_search";
    pub const EXTRACTION_DECISION: &str = "extraction_decision";
    pub const TOOL_FAILURE: &str = "tool_failure";
    pub const SESSION_STATE_CHANGE: &str = "session_state_change";
    pub const NEXUS_PANIC: &str = "nexus_panic";
    pub const DEPLOY_EVENT: &str = "deploy_event";
}

/// Severity level constants
pub mod severity {
    pub const INFO: &str = "info";
    pub const WARNING: &str = "warning";
    pub const ERROR: &str = "error";
}

/// Emit a system event. Fire-and-forget — warns on failure, never panics.
pub async fn emit_event(
    db: &PgPool,
    event_type: &str,
    sev: &str,
    payload: Value,
) {
    match sqlx::query(
        r#"INSERT INTO system_events (event_type, severity, payload) VALUES ($1, $2, $3)"#,
    )
    .bind(event_type)
    .bind(sev)
    .bind(&payload)
    .execute(db)
    .await
    {
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(
                "system_events emit failed (event_type={}, severity={}): {}",
                event_type, sev, e
            );
        }
    }
}

/// Emit a memory_write event
pub async fn emit_memory_write(
    db: &PgPool,
    actor_id: Uuid,
    memory_id: Uuid,
    memory_type: &str,
    title: &str,
) {
    let payload = json!({
        "actor_id": actor_id.to_string(),
        "memory_id": memory_id.to_string(),
        "memory_type": memory_type,
        "title": title,
        "success": true,
    });
    emit_event(db, event_type::MEMORY_WRITE, severity::INFO, payload).await;
}

/// Emit a tool_failure event
pub async fn emit_tool_failure(
    db: &PgPool,
    tool_name: &str,
    error: &str,
    input_preview: Option<&str>,
) {
    let payload = json!({
        "tool_name": tool_name,
        "error": error,
        "input_preview": input_preview,
    });
    emit_event(db, event_type::TOOL_FAILURE, severity::ERROR, payload).await;
}

/// Emit a memory_search event
pub async fn emit_memory_search(
    db: &PgPool,
    query: &str,
    namespace: &str,
    result_count: usize,
) {
    let payload = json!({
        "query": query,
        "namespace": namespace,
        "result_count": result_count,
    });
    emit_event(db, event_type::MEMORY_SEARCH, severity::INFO, payload).await;
}

/// Emit an extraction_decision event
pub async fn emit_extraction_decision(
    db: &PgPool,
    user_msg_preview: &str,
    worth_storing: bool,
    memory_type: Option<&str>,
    title: Option<&str>,
) {
    let payload = json!({
        "user_msg_preview": &user_msg_preview[..user_msg_preview.len().min(100)],
        "worth_storing": worth_storing,
        "memory_type": memory_type,
        "title": title,
    });
    emit_event(db, event_type::EXTRACTION_DECISION, severity::INFO, payload).await;
}

/// Emit a session_state_change event
pub async fn emit_session_state_change(
    db: &PgPool,
    session_id: Uuid,
    from_state: &str,
    to_state: &str,
) {
    let payload = json!({
        "session_id": session_id.to_string(),
        "from_state": from_state,
        "to_state": to_state,
    });
    emit_event(db, event_type::SESSION_STATE_CHANGE, severity::INFO, payload).await;
}

/// Emit a deploy_event
pub async fn emit_deploy(
    db: &PgPool,
    label: &str,
    success: bool,
    detail: Option<&str>,
) {
    let sev = if success { severity::INFO } else { severity::ERROR };
    let payload = json!({
        "label": label,
        "success": success,
        "detail": detail,
    });
    emit_event(db, event_type::DEPLOY_EVENT, sev, payload).await;
}

/// Emit a nexus_panic event
pub async fn emit_nexus_panic(db: &PgPool, detail: &str) {
    let payload = json!({ "detail": detail });
    emit_event(db, event_type::NEXUS_PANIC, severity::ERROR, payload).await;
}
