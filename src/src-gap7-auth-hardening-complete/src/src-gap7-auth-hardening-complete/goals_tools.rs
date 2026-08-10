//! Goals and Planning tools — Gap 2
//! Gives SuperFrank durable goal tracking with step-level progress.

use anyhow::Result;
use serde_json::{json, Value};
use uuid::Uuid;
use tracing::info;
use crate::tools::ToolContext;

// ── goal_create ────────────────────────────────────────────────────────────────

pub async fn exec_goal_create(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let title       = input["title"].as_str().unwrap_or("").to_string();
    let description = input["description"].as_str().unwrap_or("").to_string();
    let priority    = input["priority"].as_i64().unwrap_or(5) as i32;
    let context_val = input.get("context").cloned().unwrap_or(Value::Null);

    if title.is_empty() {
        return Ok(json!({ "error": "title is required" }));
    }

    let row = sqlx::query!(
        r#"
        INSERT INTO frank_goals (user_id, title, description, priority, context)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, title, status, priority, created_at
        "#,
        ctx.user_id,
        title,
        description,
        priority,
        context_val,
    )
    .fetch_one(&ctx.db)
    .await?;

    info!("Goal created: {} ({})", row.title, row.id);

    Ok(json!({
        "goal_id":    row.id.to_string(),
        "title":      row.title,
        "status":     row.status,
        "priority":   row.priority,
        "created_at": row.created_at.to_string(),
        "message":    format!("Goal '{}' created.", row.title),
    }))
}

// ── goal_update ────────────────────────────────────────────────────────────────

pub async fn exec_goal_update(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let goal_id_str = input["goal_id"].as_str().unwrap_or("");
    let goal_id: Uuid = goal_id_str.parse().map_err(|_| anyhow::anyhow!("Invalid goal_id"))?;

    // Build dynamic update — only set fields that were provided
    let title       = input.get("title").and_then(|v| v.as_str()).map(|s| s.to_string());
    let description = input.get("description").and_then(|v| v.as_str()).map(|s| s.to_string());
    let status      = input.get("status").and_then(|v| v.as_str()).map(|s| s.to_string());
    let priority    = input.get("priority").and_then(|v| v.as_i64()).map(|n| n as i32);

    let row = sqlx::query!(
        r#"
        UPDATE frank_goals SET
            title       = COALESCE($1, title),
            description = COALESCE($2, description),
            status      = COALESCE($3, status),
            priority    = COALESCE($4, priority),
            updated_at  = NOW()
        WHERE id = $5 AND user_id = $6
        RETURNING id, title, status, priority, updated_at
        "#,
        title,
        description,
        status,
        priority,
        goal_id,
        ctx.user_id,
    )
    .fetch_optional(&ctx.db)
    .await?;

    match row {
        None => Ok(json!({ "error": "Goal not found" })),
        Some(r) => Ok(json!({
            "goal_id":    r.id.to_string(),
            "title":      r.title,
            "status":     r.status,
            "priority":   r.priority,
            "updated_at": r.updated_at.to_string(),
            "message":    "Goal updated.",
        })),
    }
}

// ── goal_list ──────────────────────────────────────────────────────────────────

pub async fn exec_goal_list(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let status_filter = input.get("status").and_then(|v| v.as_str()).unwrap_or("active");

    let goals = sqlx::query!(
        r#"
        SELECT
            g.id, g.title, g.description, g.status, g.priority,
            g.created_at, g.updated_at, g.completed_at,
            COUNT(s.id) as step_count,
            COUNT(s.id) FILTER (WHERE s.status = 'complete') as steps_done
        FROM frank_goals g
        LEFT JOIN frank_plan_steps s ON s.goal_id = g.id
        WHERE g.user_id = $1 AND g.status = $2
        GROUP BY g.id
        ORDER BY g.priority DESC, g.created_at ASC
        "#,
        ctx.user_id,
        status_filter,
    )
    .fetch_all(&ctx.db)
    .await?;

    let items: Vec<Value> = goals.iter().map(|g| {
        let step_count = g.step_count.unwrap_or(0);
        let steps_done = g.steps_done.unwrap_or(0);
        let progress = if step_count > 0 {
            format!("{}/{} steps complete", steps_done, step_count)
        } else {
            "no steps defined".to_string()
        };
        json!({
            "goal_id":      g.id.to_string(),
            "title":        g.title,
            "description":  g.description,
            "status":       g.status,
            "priority":     g.priority,
            "progress":     progress,
            "step_count":   step_count,
            "steps_done":   steps_done,
            "created_at":   g.created_at.to_string(),
            "completed_at": g.completed_at.as_ref().map(|t| t.to_string()),
        })
    }).collect();

    Ok(json!({
        "status_filter": status_filter,
        "count": items.len(),
        "goals": items,
    }))
}

// ── goal_complete ──────────────────────────────────────────────────────────────

pub async fn exec_goal_complete(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let goal_id_str = input["goal_id"].as_str().unwrap_or("");
    let goal_id: Uuid = goal_id_str.parse().map_err(|_| anyhow::anyhow!("Invalid goal_id"))?;
    let notes = input.get("notes").and_then(|v| v.as_str()).map(|s| s.to_string());

    // Store notes as a context update if provided
    let row = sqlx::query!(
        r#"
        UPDATE frank_goals SET
            status       = 'complete',
            context      = CASE
                WHEN $1::TEXT IS NOT NULL
                THEN jsonb_set(COALESCE(context, '{}'), '{completion_notes}', to_jsonb($1::TEXT))
                ELSE context
            END,
            updated_at   = NOW(),
            completed_at = NOW()
        WHERE id = $2 AND user_id = $3
        RETURNING id, title, completed_at
        "#,
        notes,
        goal_id,
        ctx.user_id,
    )
    .fetch_optional(&ctx.db)
    .await?;

    match row {
        None => Ok(json!({ "error": "Goal not found" })),
        Some(r) => {
            info!("Goal completed: {} ({})", r.title, r.id);
            Ok(json!({
                "goal_id":      r.id.to_string(),
                "title":        r.title,
                "status":       "complete",
                "completed_at": r.completed_at.as_ref().map(|t| t.to_string()),
                "message":      format!("Goal '{}' marked complete.", r.title),
            }))
        }
    }
}

// ── plan_set ───────────────────────────────────────────────────────────────────

pub async fn exec_plan_set(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let goal_id_str = input["goal_id"].as_str().unwrap_or("");
    let goal_id: Uuid = goal_id_str.parse().map_err(|_| anyhow::anyhow!("Invalid goal_id"))?;

    let steps_arr = input["steps"].as_array().ok_or_else(|| anyhow::anyhow!("steps must be an array"))?;

    // Verify goal belongs to user
    let exists = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM frank_goals WHERE id = $1 AND user_id = $2",
        goal_id,
        ctx.user_id,
    )
    .fetch_one(&ctx.db)
    .await?;

    if exists.unwrap_or(0) == 0 {
        return Ok(json!({ "error": "Goal not found" }));
    }

    // Replace all steps atomically
    let mut tx = ctx.db.begin().await?;

    sqlx::query!("DELETE FROM frank_plan_steps WHERE goal_id = $1", goal_id)
        .execute(&mut *tx)
        .await?;

    let mut created = Vec::new();
    for step_val in steps_arr {
        let step_number  = step_val["step_number"].as_i64().unwrap_or(0) as i32;
        let title        = step_val["title"].as_str().unwrap_or("").to_string();
        let description  = step_val.get("description").and_then(|v| v.as_str()).map(|s| s.to_string());

        let row = sqlx::query!(
            r#"
            INSERT INTO frank_plan_steps (goal_id, step_number, title, description)
            VALUES ($1, $2, $3, $4)
            RETURNING id, step_number, title, status
            "#,
            goal_id,
            step_number,
            title,
            description,
        )
        .fetch_one(&mut *tx)
        .await?;

        created.push(json!({
            "step_id":     row.id.to_string(),
            "step_number": row.step_number,
            "title":       row.title,
            "status":      row.status,
        }));
    }

    tx.commit().await?;

    info!("Plan set for goal {}: {} steps", goal_id, created.len());

    Ok(json!({
        "goal_id": goal_id.to_string(),
        "steps_created": created.len(),
        "steps": created,
        "message": format!("{} steps set for goal.", created.len()),
    }))
}

// ── plan_step_update ───────────────────────────────────────────────────────────

pub async fn exec_plan_step_update(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let step_id_str = input["step_id"].as_str().unwrap_or("");
    let step_id: Uuid = step_id_str.parse().map_err(|_| anyhow::anyhow!("Invalid step_id"))?;
    let status = input["status"].as_str().unwrap_or("").to_string();
    let notes  = input.get("notes").and_then(|v| v.as_str()).map(|s| s.to_string());

    if status.is_empty() {
        return Ok(json!({ "error": "status is required" }));
    }

    let completed_now = status == "complete";

    let row = sqlx::query!(
        r#"
        UPDATE frank_plan_steps SET
            status       = $1,
            notes        = COALESCE($2, notes),
            updated_at   = NOW(),
            completed_at = CASE WHEN $3 THEN NOW() ELSE completed_at END
        WHERE id = $4
        RETURNING id, step_number, title, status, goal_id
        "#,
        status,
        notes,
        completed_now,
        step_id,
    )
    .fetch_optional(&ctx.db)
    .await?;

    match row {
        None => Ok(json!({ "error": "Step not found" })),
        Some(r) => {
            let goal_id = r.goal_id;
            let response = json!({
                "step_id":     r.id.to_string(),
                "goal_id":     r.goal_id.to_string(),
                "step_number": r.step_number,
                "title":       r.title,
                "status":      r.status,
                "message":     format!("Step {} updated to '{}'.", r.step_number, r.status),
            });

            // ── Nexus Auto-Continuation ──────────────────────────────────────
            // If this step just completed, check for next pending step and fire a continuation
            if completed_now {
                let next_step = sqlx::query!(
                    r#"
                    SELECT id, step_number, title
                    FROM frank_plan_steps
                    WHERE goal_id = $1 AND status = 'pending'
                    ORDER BY step_number ASC
                    LIMIT 1
                    "#,
                    goal_id,
                )
                .fetch_optional(&ctx.db)
                .await?;

                if let Some(next) = next_step {
                    // Fire a Nexus trigger to continue immediately
                    info!(
                        "[Nexus Auto-Continue] Step {} complete, next is step {} — firing continuation",
                        r.step_number, next.step_number
                    );

                    let trigger_id = Uuid::new_v4();
                    let now = chrono::Utc::now();
                    let schedule_json = json!({ "type": "once", "at": now });
                    let payload_json = json!({
                        "type": "agent_turn",
                        "prompt": format!(
                            "Step {} complete. Continue with step {}: {}",
                            r.step_number, next.step_number, next.title
                        ),
                        "model": "sonnet",
                    });

                    let _ = sqlx::query(
                        "INSERT INTO frank_triggers
                         (id, name, schedule, payload, user_id, max_fires, next_fire_at, enabled)
                         VALUES ($1, $2, $3, $4, $5, $6, $7, true)"
                    )
                    .bind(trigger_id)
                    .bind(format!("auto-continue-goal-{}", goal_id))
                    .bind(schedule_json)
                    .bind(payload_json)
                    .bind(ctx.user_id)
                    .bind(1) // one-shot
                    .bind(now)
                    .execute(&ctx.db)
                    .await;
                }
            }
            // ─────────────────────────────────────────────────────────────────

            Ok(response)
        }
    }
}

// ── active goals for system prompt injection ───────────────────────────────────

pub async fn load_active_goals_for_prompt(db: &sqlx::PgPool, user_id: Uuid) -> String {
    let goals = sqlx::query!(
        r#"
        SELECT g.id, g.title, g.description, g.priority, g.status
        FROM frank_goals g
        WHERE g.user_id = $1 AND g.status = 'active'
        ORDER BY g.priority DESC, g.created_at ASC
        LIMIT 10
        "#,
        user_id,
    )
    .fetch_all(db)
    .await;

    let goals = match goals {
        Ok(g) => g,
        Err(_) => return String::new(),
    };

    if goals.is_empty() {
        return String::new();
    }

    let mut out = String::from("## Active Goals\n");

    for goal in &goals {
        let steps = sqlx::query!(
            r#"
            SELECT id, step_number, title, status
            FROM frank_plan_steps
            WHERE goal_id = $1
            ORDER BY step_number ASC
            "#,
            goal.id,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default();

        out.push_str(&format!(
            "\n**Goal (priority {}):** {}\n{}\n",
            goal.priority, goal.title, goal.description
        ));

        if steps.is_empty() {
            out.push_str("  _(no steps defined yet)_\n");
        } else {
            for step in &steps {
                let icon = match step.status.as_str() {
                    "complete"    => "✅",
                    "in_progress" => "🔄",
                    "blocked"     => "🚫",
                    "skipped"     => "⏭️",
                    _             => "⬜",
                };
                out.push_str(&format!(
                    "  {} Step {}: {}\n",
                    icon, step.step_number, step.title
                ));
            }
        }
    }

    out
}
