//! Plan Continuation Hook — Nexus auto-continuation for completed plan steps
//!
//! When a plan step is marked complete, this hook checks if there are more pending steps
//! and schedules a Nexus trigger to auto-continue the work.

use anyhow::Result;
use serde_json::Value;
use sqlx::PgPool;
use tracing::info;
use uuid::Uuid;

pub async fn maybe_auto_continue(
    db: &PgPool,
    user_id: Uuid,
    goal_id: Uuid,
    step_status: &str,
) -> Result<()> {
    // Only trigger on completion
    if step_status != "complete" {
        return Ok(());
    }

    // Check if there are more pending steps
    let pending = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM frank_plan_steps WHERE goal_id = $1 AND status = 'pending'",
        goal_id
    )
    .fetch_one(db)
    .await?
    .unwrap_or(0);

    if pending == 0 {
        return Ok(()); // No more work — done
    }

    // Get goal title for the prompt
    let goal_title = sqlx::query_scalar!(
        "SELECT title FROM frank_goals WHERE id = $1",
        goal_id
    )
    .fetch_one(db)
    .await
    .unwrap_or_else(|_| "Goal".to_string());

    // Fire in 2 seconds — just enough delay for DB state to settle
    let next_fire = chrono::Utc::now() + chrono::Duration::seconds(2);

    let trigger_id = crate::nexus::create_trigger(
        db,
        &format!("Auto-continue: {}", goal_title),
        &crate::nexus::TriggerSchedule::Once { at: next_fire },
        &crate::nexus::TriggerPayload::AgentTurn {
            prompt: format!("Continue work on goal: {}", goal_title),
            model: Some("sonnet".to_string()),
            tools: None,
        },
        Some(user_id),
        1, // one-shot
    )
    .await?;

    info!("Auto-continuation scheduled for goal '{}' (trigger_id={})", goal_title, trigger_id);
    Ok(())
}
