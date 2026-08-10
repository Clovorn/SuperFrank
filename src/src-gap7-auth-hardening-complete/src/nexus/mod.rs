//! The Nexus — SuperFrank's persistent event engine.
//!
//! Unlike OpenClaw's heartbeat-poll model, the Nexus runs as a live Tokio task
//! alongside the HTTP server. It evaluates triggers every 500ms and fires them
//! with <1s latency. Triggers can be time-based (cron, interval, once) or
//! reactive (webhook arrival, agent completion, file change).
//!
//! This is Frank's nervous system — it keeps him alive and acting even when
//! no one is talking to him.

use anyhow::Result;
use chrono::{DateTime, Utc};
use cron::Schedule;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};
use uuid::Uuid;
use futures::FutureExt; // For catch_unwind

use crate::delivery::DeliveryBus;
use crate::swarm::Swarm;
use crate::tools::{execute_tool, ToolContext};

// ── Trigger types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerSchedule {
    /// Fire once at an absolute UTC time
    Once { at: DateTime<Utc> },
    /// Fire on a cron expression (e.g. "0 9 * * 1-5" = weekdays at 9am)
    Cron { expr: String },
    /// Fire every N milliseconds
    IntervalMs { ms: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TriggerPayload {
    /// Run a full agent turn with the given prompt
    AgentTurn {
        prompt: String,
        model: Option<String>,
        tools: Option<Vec<String>>,
    },
    /// Deliver a plain message to the user
    Notify {
        title: String,
        body: String,
    },
    /// Fire an outbound webhook
    Webhook {
        url: String,
        secret: Option<String>,
        body: Value,
    },
    /// Execute a tool directly without LLM (Gap 6 Option B)
    DirectTool {
        tool: String,
        input: Value,
    },
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct TriggerRow {
    id: Uuid,
    name: String,
    schedule: Value,
    payload: Value,
    user_id: Option<Uuid>,
    enabled: bool,
    fire_count: i32,
    max_fires: i32,
    next_fire_at: Option<DateTime<Utc>>,
}

// ── Nexus ─────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Nexus {
    db: PgPool,
    swarm: Arc<Swarm>,
    delivery: Arc<DeliveryBus>,
}

impl Nexus {
    pub fn new(db: PgPool, swarm: Arc<Swarm>, delivery: Arc<DeliveryBus>) -> Self {
        Self { db, swarm, delivery }
    }

    /// Spawn the Nexus as a background Tokio task. Never returns.
    /// Gap 8B.5: Panic recovery — if a tick panics, log it and restart the loop
    pub async fn run(self) {
        info!("[Nexus] Starting — tick interval 500ms");
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut panic_count = 0;
        let mut last_panic = std::time::Instant::now();

        loop {
            interval.tick().await;

            // Wrap tick in panic handler
            let result = std::panic::AssertUnwindSafe(self.evaluate_triggers())
                .catch_unwind()
                .await;

            match result {
                Ok(Ok(_)) => {
                    // Successful tick — reset panic counter if it's been >60s since last panic
                    if panic_count > 0 && last_panic.elapsed().as_secs() > 60 {
                        panic_count = 0;
                    }
                }
                Ok(Err(e)) => {
                    warn!("[Nexus] Trigger evaluation error: {}", e);
                }
                Err(panic_info) => {
                    error!("[Nexus] PANIC in tick loop: {:?}", panic_info);
                    panic_count += 1;
                    last_panic = std::time::Instant::now();

                    // If >3 panics in 60 seconds, stop Nexus entirely (health will go red)
                    if panic_count >= 3 {
                        error!("[Nexus] Too many panics ({} in 60s) — shutting down", panic_count);
                        break;
                    }

                    // Sleep 5 seconds before restarting loop
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    warn!("[Nexus] Recovered from panic, resuming ticks");
                }
            }
        }

        error!("[Nexus] Nexus stopped due to repeated panics — manual restart required");
    }

    async fn evaluate_triggers(&self) -> Result<()> {
        let now = Utc::now();

        // Single indexed query — only rows that are due
        let rows = sqlx::query_as::<_, TriggerRow>(
            "SELECT id, name, schedule, payload, user_id, enabled,
                    fire_count, max_fires, next_fire_at
             FROM frank_triggers
             WHERE enabled = true
               AND (next_fire_at IS NULL OR next_fire_at <= $1)
               AND (max_fires = 0 OR fire_count < max_fires)
             ORDER BY next_fire_at ASC NULLS FIRST
             LIMIT 20"
        )
        .bind(now)
        .fetch_all(&self.db)
        .await?;

        for row in rows {
            let nexus = self.clone();
            tokio::spawn(async move {
                if let Err(e) = nexus.fire(&row).await {
                    error!("[Nexus] Fire failed for trigger '{}': {}", row.name, e);
                }
            });
        }

        Ok(())
    }

    async fn fire(&self, row: &TriggerRow) -> Result<()> {
        info!("[Nexus] Firing trigger '{}' (id={})", row.name, row.id);

        // Optimistic-lock: mark as firing to prevent double-fire
        let affected = sqlx::query(
            "UPDATE frank_triggers
             SET fire_count = fire_count + 1,
                 last_fired = NOW(),
                 next_fire_at = $1
             WHERE id = $2
               AND (next_fire_at IS NULL OR next_fire_at <= NOW())"
        )
        .bind(self.compute_next_fire(row)?)
        .bind(row.id)
        .execute(&self.db)
        .await?
        .rows_affected();

        if affected == 0 {
            // Another Nexus tick already grabbed this one — skip
            return Ok(());
        }

        // Disable one-shot triggers after firing
        if row.max_fires > 0 && (row.fire_count + 1) >= row.max_fires {
            sqlx::query("UPDATE frank_triggers SET enabled = false WHERE id = $1")
                .bind(row.id)
                .execute(&self.db)
                .await?;
        }

        // Dispatch payload
        let payload: TriggerPayload = serde_json::from_value(row.payload.clone())
            .map_err(|e| anyhow::anyhow!("Bad trigger payload: {}", e))?;

        match payload {
            TriggerPayload::AgentTurn { prompt, model, tools } => {
                let user_id = row.user_id.unwrap_or_default();
                self.swarm.spawn_nexus_agent(
                    row.id,
                    &row.name,
                    &prompt,
                    model.as_deref().unwrap_or("sonnet"),
                    tools.as_deref().unwrap_or(&[]),
                    user_id,
                    self.delivery.clone(),
                ).await?;
            }
            TriggerPayload::Notify { title, body } => {
                if let Some(user_id) = row.user_id {
                    self.delivery.notify_user(user_id, &title, &body).await?;
                }
            }
            TriggerPayload::Webhook { url, secret, body } => {
                self.delivery.fire_webhook(&url, secret.as_deref(), &body).await?;
            }
            TriggerPayload::DirectTool { tool, input } => {
                let ctx = ToolContext {
                    db: self.db.clone(),
                    session_id: Uuid::nil(),
                    user_id: row.user_id.unwrap_or_default(),
                    chat_bucket: "work".to_string(),
                    chat_folder: None,
                    brave_api_key: None,
                    google_ai_key: None,
                    google_ai_project: None,
                    luma_api_key: None,
                    openai_api_key: None,
                    forge: None,
                };
                let result = execute_tool(&tool, &input, &ctx).await;
                info!("[Nexus] DirectTool result: {:?}", result);
            }
        }

        Ok(())
    }

    fn compute_next_fire(&self, row: &TriggerRow) -> Result<Option<DateTime<Utc>>> {
        let sched: TriggerSchedule = serde_json::from_value(row.schedule.clone())
            .map_err(|e| anyhow::anyhow!("Bad schedule: {}", e))?;

        let next = match sched {
            TriggerSchedule::Once { .. } => None, // one-shot: no next
            TriggerSchedule::Cron { expr } => {
                let schedule = Schedule::from_str(&expr)
                    .map_err(|e| anyhow::anyhow!("Bad cron expr '{}': {:?}", expr, e))?;
                schedule.upcoming(Utc).next()
            }
            TriggerSchedule::IntervalMs { ms } => {
                Some(Utc::now() + chrono::Duration::milliseconds(ms as i64))
            }
        };

        Ok(next)
    }
}

// ── Public API helpers (called from tool + HTTP handlers) ─────────────────────

pub async fn create_trigger(
    db: &PgPool,
    name: &str,
    schedule: &TriggerSchedule,
    payload: &TriggerPayload,
    user_id: Option<Uuid>,
    max_fires: i32,
) -> Result<Uuid> {
    let id = Uuid::new_v4();
    let schedule_json = serde_json::to_value(schedule)?;
    let payload_json = serde_json::to_value(payload)?;

    // Compute first fire time
    let next_fire = match schedule {
        TriggerSchedule::Once { at } => Some(*at),
        TriggerSchedule::Cron { expr } => {
            let s = Schedule::from_str(expr)
                .map_err(|e| anyhow::anyhow!("Bad cron: {:?}", e))?;
            s.upcoming(Utc).next()
        }
        TriggerSchedule::IntervalMs { ms } => {
            Some(Utc::now() + chrono::Duration::milliseconds(*ms as i64))
        }
    };

    sqlx::query(
        "INSERT INTO frank_triggers
         (id, name, schedule, payload, user_id, max_fires, next_fire_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)"
    )
    .bind(id)
    .bind(name)
    .bind(schedule_json)
    .bind(payload_json)
    .bind(user_id)
    .bind(max_fires)
    .bind(next_fire)
    .execute(db)
    .await?;

    info!("[Nexus] Trigger '{}' created — next fire: {:?}", name, next_fire);
    Ok(id)
}

pub async fn list_triggers(db: &PgPool, user_id: Option<Uuid>) -> Result<Vec<Value>> {
    let rows = if let Some(uid) = user_id {
        sqlx::query_as::<_, TriggerRow>(
            "SELECT id, name, schedule, payload, user_id, enabled, fire_count, max_fires, next_fire_at
             FROM frank_triggers WHERE user_id = $1 ORDER BY created_at DESC"
        ).bind(uid).fetch_all(db).await?
    } else {
        sqlx::query_as::<_, TriggerRow>(
            "SELECT id, name, schedule, payload, user_id, enabled, fire_count, max_fires, next_fire_at
             FROM frank_triggers ORDER BY created_at DESC"
        ).fetch_all(db).await?
    };

    Ok(rows.iter().map(|r| json!({
        "id": r.id,
        "name": r.name,
        "schedule": r.schedule,
        "payload": r.payload,
        "enabled": r.enabled,
        "fire_count": r.fire_count,
        "max_fires": r.max_fires,
        "next_fire_at": r.next_fire_at,
    })).collect())
}

pub async fn delete_trigger(db: &PgPool, id: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM frank_triggers WHERE id = $1")
        .bind(id).execute(db).await?;
    Ok(())
}

pub async fn toggle_trigger(db: &PgPool, id: Uuid, enabled: bool) -> Result<()> {
    sqlx::query("UPDATE frank_triggers SET enabled = $1 WHERE id = $2")
        .bind(enabled).bind(id).execute(db).await?;
    Ok(())
}
