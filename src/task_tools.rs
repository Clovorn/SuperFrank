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

    // Write task completion to build_history memory (non-blocking)
    let task_row = sqlx::query!(
        "SELECT title, description, context FROM frank_tasks WHERE task_id = $1",
        task_id
    )
    .fetch_optional(db)
    .await;

    if let Ok(Some(row)) = task_row {
        // Validate and enrich build history entry
        let mut memory_tags = vec!["engineer_task".to_string(), "build_history".to_string()];

        // Extract gap label from title (format: "Gap X · PYZ — Title")
        let gap_label = if let Some(idx) = row.title.find(" — ") {
            row.title[..idx].to_string()
        } else if let Some(idx) = row.title.find(':') {
            row.title[..idx].trim().to_string()
        } else {
            "Unknown".to_string()
        };

        // Auto-extract gap tag from gap_label
        if gap_label != "Unknown" {
            // Extract gap number: "Gap 8 · P8A" -> "gap8"
            let gap_tag = gap_label
                .split("·")
                .next()
                .unwrap_or(&gap_label)
                .trim()
                .to_lowercase()
                .replace(" ", "");
            memory_tags.push(gap_tag);
        }

        // Validate that outcome is not empty (this is our verification evidence)
        if outcome.trim().is_empty() {
            tracing::warn!("Task {} completed with empty outcome — memory entry will lack verification", task_id);
        }

        let memory_title = format!("{} — {}", gap_label, row.title);
        
        // Extract files_touched from context if present
        let files_touched = row.context
            .as_ref()
            .and_then(|ctx| ctx.get("files_touched"))
            .and_then(|v| v.as_str())
            .unwrap_or("None specified");

        let memory_content = format!(
            "**Task ID:** {}\n\n**Files Modified:**\n{}\n\n**Verification Outcome:**\n```\n{}\n```\n\n**Description:**\n{}\n",
            task_id_str,
            files_touched,
            outcome,
            row.description.as_deref().unwrap_or("No description provided")
        );

        if let Err(e) = crate::memory::store(
            db,
            "build_history",
            "chuck_frank",
            "concept",
            &memory_title,
            &memory_content,
            6,
            &memory_tags,
            None,
            None,
            None,
            "engineer_task_done",
        ).await {
            tracing::warn!("Failed to write build history memory entry: {}", e);
        } else {
            info!("Wrote build history memory entry: {} (tags: {:?})", memory_title, memory_tags);
        }
    }

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
