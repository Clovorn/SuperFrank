//! Task management tools for Engineer agent
//! Provides atomic claim/complete/block/list operations on frank_tasks table
//! Engineer calls these in its agentic loop to work through the task queue.

use anyhow::Result;
use serde_json::{json, Value};
use sqlx::PgPool;
use tracing::info;

/// List all PENDING tasks assigned to Engineer, ordered by priority DESC
pub async fn exec_task_list_pending(db: &PgPool) -> Result<Value> {
    let rows = sqlx::query!(
        r#"SELECT task_id, title, description, priority, tags, context
           FROM frank_tasks
           WHERE status = 'PENDING' AND (assigned_to = 'Engineer' OR assigned_to IS NULL)
           ORDER BY priority DESC, created_at ASC
           LIMIT 20"#
    )
    .fetch_all(db)
    .await?;

    let tasks: Vec<Value> = rows.iter().map(|r| json!({
        "task_id": r.task_id.to_string(),
        "title": r.title,
        "description": r.description,
        "priority": r.priority,
        "tags": r.tags,
        "context": r.context,
    })).collect();

    Ok(json!({
        "count": tasks.len(),
        "tasks": tasks,
    }))
}

/// Atomically claim a task — sets status IN_PROGRESS, returns task details
/// Use this before starting work on a task.
pub async fn exec_task_claim(input: &Value, db: &PgPool) -> Result<Value> {
    let task_id_str = input["task_id"].as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing task_id"))?;
    let task_id: uuid::Uuid = task_id_str.parse()
        .map_err(|_| anyhow::anyhow!("Invalid task_id UUID"))?;

    let updated = sqlx::query!(
        r#"UPDATE frank_tasks
           SET status = 'IN_PROGRESS', started_at = NOW(), updated_at = NOW()
           WHERE task_id = $1 AND status = 'PENDING'
           RETURNING task_id, title, description, priority, context"#,
        task_id
    )
    .fetch_optional(db)
    .await?;

    match updated {
        Some(row) => {
            info!("Task claimed: {} ({})", row.title, row.task_id);
            Ok(json!({
                "success": true,
                "task_id": row.task_id.to_string(),
                "title": row.title,
                "description": row.description,
                "priority": row.priority,
                "context": row.context,
            }))
        }
        None => Ok(json!({
            "success": false,
            "error": "Task not found or already claimed by another agent",
            "task_id": task_id_str,
        }))
    }
}

/// Mark a task COMPLETE with an outcome summary
pub async fn exec_task_done(input: &Value, db: &PgPool) -> Result<Value> {
    let task_id_str = input["task_id"].as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing task_id"))?;
    let task_id: uuid::Uuid = task_id_str.parse()
        .map_err(|_| anyhow::anyhow!("Invalid task_id UUID"))?;
    let outcome = input["outcome"].as_str().unwrap_or("Completed");

    // Store outcome in context jsonb
    let ctx_update = json!({ "outcome": outcome, "completed_by": "Engineer" });

    sqlx::query!(
        r#"UPDATE frank_tasks
           SET status = 'COMPLETE', completed_at = NOW(), updated_at = NOW(),
               context = COALESCE(context, '{}'::jsonb) || $2::jsonb
           WHERE task_id = $1"#,
        task_id,
        ctx_update
    )
    .execute(db)
    .await?;

    info!("Task completed: {} — {}", task_id, outcome);
    Ok(json!({
        "success": true,
        "task_id": task_id_str,
        "status": "COMPLETE",
        "outcome": outcome,
    }))
}

/// Mark a task BLOCKED with a reason — escalates to Mac Frank via FRANK_TO_MAC.md
pub async fn exec_task_block(input: &Value, db: &PgPool) -> Result<Value> {
    let task_id_str = input["task_id"].as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing task_id"))?;
    let task_id: uuid::Uuid = task_id_str.parse()
        .map_err(|_| anyhow::anyhow!("Invalid task_id UUID"))?;
    let reason = input["reason"].as_str().unwrap_or("Unknown blocker");

    let ctx_update = json!({ "blocked_reason": reason, "blocked_by": "Engineer" });

    sqlx::query!(
        r#"UPDATE frank_tasks
           SET status = 'BLOCKED', updated_at = NOW(),
               context = COALESCE(context, '{}'::jsonb) || $2::jsonb
           WHERE task_id = $1"#,
        task_id,
        ctx_update
    )
    .execute(db)
    .await?;

    // Write blocker to FRANK_TO_MAC.md
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
    let mac_msg = format!(
        "# Engineer → Mac Frank: BLOCKED\n\n**Time:** {}\n**Task:** {}\n**Reason:** {}\n\n**Action needed:** Review blocker and unblock or reassign task.\n",
        now, task_id_str, reason
    );
    if let Err(e) = tokio::fs::write("/opt/frankos/workspace/COLLAB/FRANK_TO_MAC.md", &mac_msg).await {
        tracing::warn!("Failed to write FRANK_TO_MAC.md: {}", e);
    }

    info!("Task blocked: {} — {}", task_id, reason);
    Ok(json!({
        "success": true,
        "task_id": task_id_str,
        "status": "BLOCKED",
        "reason": reason,
        "escalated_to": "FRANK_TO_MAC.md",
    }))
}
