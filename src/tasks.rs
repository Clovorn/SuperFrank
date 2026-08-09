//! Task management — CRUD for the FrankOS task orchestration system

use axum::{
    extract::{Path, Query, State},
    response::Json,
    routing::{get, patch, post},
    Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn tasks_router() -> Router<AppState> {
    Router::new()
        .route("/admin/tasks",      get(list_tasks).post(create_task))
        .route("/admin/tasks/:id",  get(get_task).patch(update_task))
}

// ── Query params ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TaskListParams {
    pub status: Option<String>,
    pub assigned_to: Option<String>,
    pub parent_task_id: Option<Uuid>,
}

// ── GET /admin/tasks ──────────────────────────────────────────────────────────

pub async fn list_tasks(
    State(state): State<AppState>,
    Query(params): Query<TaskListParams>,
) -> Json<Value> {
    let mut conditions = vec!["1=1"];
    // Build dynamic query with fixed filters (sqlx requires compile-time queries,
    // so we use a hand-built string here for flexibility)
    let status_filter = params.status.clone().unwrap_or_default();
    let assigned_filter = params.assigned_to.clone().unwrap_or_default();
    let parent_filter = params.parent_task_id;

    let query_str = format!(
        r#"SELECT id, title, description, status, priority, assigned_to,
                  created_at, updated_at, completed_at, context,
                  parent_task_id, blocked_reason, result_location
           FROM tasks
           WHERE ($1::text = '' OR status = $1)
             AND ($2::text = '' OR assigned_to = $2)
             AND ($3::uuid IS NULL OR parent_task_id = $3)
           ORDER BY priority DESC, updated_at DESC
           LIMIT 100"#
    );

    let rows = sqlx::query(&query_str)
        .bind(&status_filter)
        .bind(&assigned_filter)
        .bind(parent_filter)
        .fetch_all(&state.db)
        .await;

    match rows {
        Ok(rows) => {
            let tasks: Vec<Value> = rows.iter().map(|r| row_to_json(r)).collect();
            Json(json!({ "tasks": tasks, "count": tasks.len() }))
        }
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

// ── GET /admin/tasks/:id ──────────────────────────────────────────────────────

pub async fn get_task(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Json<Value> {
    let row = sqlx::query(
        r#"SELECT id, title, description, status, priority, assigned_to,
                  created_at, updated_at, completed_at, context,
                  parent_task_id, blocked_reason, result_location
           FROM tasks WHERE id = $1"#
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await;

    match row {
        Ok(Some(r)) => Json(json!({ "task": row_to_json(&r) })),
        Ok(None)    => Json(json!({ "error": "task not found" })),
        Err(e)      => Json(json!({ "error": e.to_string() })),
    }
}

// ── POST /admin/tasks ─────────────────────────────────────────────────────────

pub async fn create_task(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let title = match body.get("title").and_then(|v| v.as_str()) {
        Some(t) => t.to_string(),
        None => return Json(json!({ "error": "title is required" })),
    };

    let description    = body.get("description").and_then(|v| v.as_str()).map(String::from);
    let status         = body.get("status").and_then(|v| v.as_str()).unwrap_or("PLANNING").to_string();
    let priority: i32  = body.get("priority").and_then(|v| v.as_i64()).unwrap_or(5) as i32;
    let assigned_to    = body.get("assigned_to").and_then(|v| v.as_str()).map(String::from);
    let parent_task_id = body.get("parent_task_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    let context        = body.get("context").cloned();
    let result_location = body.get("result_location").and_then(|v| v.as_str()).map(String::from);

    let row = sqlx::query(
        r#"INSERT INTO tasks
             (title, description, status, priority, assigned_to,
              parent_task_id, context, result_location)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
           RETURNING id, title, description, status, priority, assigned_to,
                     created_at, updated_at, completed_at, context,
                     parent_task_id, blocked_reason, result_location"#
    )
    .bind(&title)
    .bind(&description)
    .bind(&status)
    .bind(priority)
    .bind(&assigned_to)
    .bind(parent_task_id)
    .bind(context.as_ref().map(|v| v.to_string()))
    .bind(&result_location)
    .fetch_one(&state.db)
    .await;

    match row {
        Ok(r)  => Json(json!({ "task": row_to_json(&r), "created": true })),
        Err(e) => Json(json!({ "error": e.to_string() })),
    }
}

// ── PATCH /admin/tasks/:id ────────────────────────────────────────────────────

pub async fn update_task(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(body): Json<Value>,
) -> Json<Value> {
    // Build dynamic update — only set fields that were provided
    let status          = body.get("status").and_then(|v| v.as_str()).map(String::from);
    let assigned_to     = body.get("assigned_to").and_then(|v| v.as_str()).map(String::from);
    let blocked_reason  = body.get("blocked_reason").and_then(|v| v.as_str()).map(String::from);
    let result_location = body.get("result_location").and_then(|v| v.as_str()).map(String::from);
    let priority: Option<i32> = body.get("priority").and_then(|v| v.as_i64()).map(|n| n as i32);
    let description     = body.get("description").and_then(|v| v.as_str()).map(String::from);

    // Set completed_at when status → COMPLETE or FAILED
    let completed_now = status.as_deref().map(|s| s == "COMPLETE" || s == "FAILED").unwrap_or(false);

    let result = sqlx::query(
        r#"UPDATE tasks SET
             status          = COALESCE($2, status),
             assigned_to     = COALESCE($3, assigned_to),
             blocked_reason  = COALESCE($4, blocked_reason),
             result_location = COALESCE($5, result_location),
             priority        = COALESCE($6, priority),
             description     = COALESCE($7, description),
             updated_at      = now(),
             completed_at    = CASE WHEN $8 THEN now() ELSE completed_at END
           WHERE id = $1
           RETURNING id, title, description, status, priority, assigned_to,
                     created_at, updated_at, completed_at, context,
                     parent_task_id, blocked_reason, result_location"#
    )
    .bind(id)
    .bind(&status)
    .bind(&assigned_to)
    .bind(&blocked_reason)
    .bind(&result_location)
    .bind(priority)
    .bind(&description)
    .bind(completed_now)
    .fetch_optional(&state.db)
    .await;

    match result {
        Ok(Some(r)) => Json(json!({ "task": row_to_json(&r), "updated": true })),
        Ok(None)    => Json(json!({ "error": "task not found" })),
        Err(e)      => Json(json!({ "error": e.to_string() })),
    }
}

// ── Helper: sqlx Row → serde_json Value ──────────────────────────────────────

fn row_to_json(r: &sqlx::postgres::PgRow) -> Value {
    let id: Uuid              = r.try_get("id").unwrap_or_default();
    let title: String         = r.try_get("title").unwrap_or_default();
    let description: Option<String> = r.try_get("description").ok().flatten();
    let status: String        = r.try_get("status").unwrap_or_default();
    let priority: i32         = r.try_get("priority").unwrap_or(5);
    let assigned_to: Option<String> = r.try_get("assigned_to").ok().flatten();
    let blocked_reason: Option<String> = r.try_get("blocked_reason").ok().flatten();
    let result_location: Option<String> = r.try_get("result_location").ok().flatten();
    let parent_task_id: Option<Uuid> = r.try_get("parent_task_id").ok().flatten();

    let created_at: Option<chrono::DateTime<chrono::Utc>> = r.try_get("created_at").ok();
    let updated_at: Option<chrono::DateTime<chrono::Utc>> = r.try_get("updated_at").ok();
    let completed_at: Option<chrono::DateTime<chrono::Utc>> = r.try_get("completed_at").ok();

    json!({
        "id": id,
        "title": title,
        "description": description,
        "status": status,
        "priority": priority,
        "assigned_to": assigned_to,
        "blocked_reason": blocked_reason,
        "result_location": result_location,
        "parent_task_id": parent_task_id,
        "created_at": created_at.map(|t| t.to_rfc3339()),
        "updated_at": updated_at.map(|t| t.to_rfc3339()),
        "completed_at": completed_at.map(|t| t.to_rfc3339()),
    })
}
