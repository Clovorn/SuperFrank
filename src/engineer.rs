//! Engineer — Persistent Resident Agent Loop
//!
//! Engineer is a dedicated Tokio task that runs continuously inside the FrankOS
//! binary from boot until shutdown. It is not spawned on-demand — it is always
//! present, watching the frank_tasks queue, spawning sub-agents for work,
//! and reporting results.
//!
//! Architecture:
//!   - Polls frank_tasks every POLL_INTERVAL_SECS for PENDING tasks
//!   - Claims tasks atomically (status → IN_PROGRESS)
//!   - Spawns a frankos_agents entry with the Engineer persistent_agent_id linked
//!   - Worker loop picks it up and executes it via agents::run_agent
//!   - Monitors completion; marks task COMPLETE or BLOCKED
//!   - Writes blockers to FRANK_TO_MAC.md
//!   - Exposes status via EngineerStatus for /api/v1/engineer/status
//!
//! The Engineer agent's identity lives in frank_persistent_agents (name=Engineer).
//! All spawned frankos_agents link back via persistent_agent_id.

use anyhow::Result;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration, Instant};
use tracing::{info, warn, error};
use uuid::Uuid;
use chrono::Utc;

/// How often Engineer polls for new tasks
const POLL_INTERVAL_SECS: u64 = 30;
/// How long to wait for a spawned agent to complete before moving on
const AGENT_TIMEOUT_SECS: u64 = 600;
/// Max concurrent Engineer sub-agents running at once
const MAX_CONCURRENT: usize = 3;

/// Engineer's persistent agent UUID — loaded from frank_persistent_agents at startup
pub const ENGINEER_NAME: &str = "Engineer";

/// Current Engineer status — readable via /api/v1/engineer/status
#[derive(Debug, Clone, serde::Serialize)]
pub struct EngineerStatus {
    pub running: bool,
    pub current_task_id: Option<Uuid>,
    pub current_task_title: Option<String>,
    pub active_agents: Vec<ActiveAgent>,
    pub tasks_completed_this_session: u32,
    pub tasks_blocked_this_session: u32,
    pub last_poll_at: Option<String>,
    pub engineer_agent_id: Option<Uuid>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ActiveAgent {
    pub agent_id: Uuid,
    pub task_id: Uuid,
    pub task_title: String,
    pub spawned_at: String,
}

/// Shared state for the Engineer loop — readable by HTTP handlers
pub type SharedEngineerStatus = Arc<RwLock<EngineerStatus>>;

impl Default for EngineerStatus {
    fn default() -> Self {
        EngineerStatus {
            running: false,
            current_task_id: None,
            current_task_title: None,
            active_agents: vec![],
            tasks_completed_this_session: 0,
            tasks_blocked_this_session: 0,
            last_poll_at: None,
            engineer_agent_id: None,
        }
    }
}

/// Create shared status — call once at startup, pass to run() and HTTP handler
pub fn new_shared_status() -> SharedEngineerStatus {
    Arc::new(RwLock::new(EngineerStatus::default()))
}

/// Main Engineer resident loop — run as a tokio::spawn'd task at gateway boot.
/// Never returns unless the process exits.
pub async fn run(db: PgPool, llm: Arc<crate::llm::LlmClient>, brave_key: Option<String>, status: SharedEngineerStatus) {
    info!("Engineer resident loop starting");

    // Load or create Engineer persistent agent ID
    let engineer_pid = match ensure_engineer_exists(&db).await {
        Ok(pid) => {
            info!("Engineer persistent agent: {}", pid);
            {
                let mut s = status.write().await;
                s.running = true;
                s.engineer_agent_id = Some(pid);
            }
            pid
        }
        Err(e) => {
            error!("Engineer: failed to load persistent agent ID: {} — loop will not run", e);
            return;
        }
    };

    let shutdown = Arc::new(AtomicBool::new(false));

    loop {
        if shutdown.load(Ordering::Relaxed) {
            info!("Engineer loop: shutdown requested");
            break;
        }

        // Update last_poll_at
        {
            let mut s = status.write().await;
            s.last_poll_at = Some(Utc::now().to_rfc3339());
        }

        // Check how many agents are currently running under Engineer
        let active_count = count_active_agents(&db, engineer_pid).await.unwrap_or(0);

        if active_count < MAX_CONCURRENT {
            match find_next_pending_task(&db).await {
                Ok(Some(task)) => {
                    info!("Engineer: found task '{}' (priority {})", task.title, task.priority);

                    // Update status
                    {
                        let mut s = status.write().await;
                        s.current_task_id = Some(task.task_id);
                        s.current_task_title = Some(task.title.clone());
                    }

                    // Claim the task
                    if let Ok(true) = claim_task(&db, task.task_id, engineer_pid).await {
                        // Spawn an agent to execute it
                        match spawn_task_agent(&db, &task, engineer_pid, brave_key.clone()).await {
                            Ok(agent_id) => {
                                info!("Engineer: spawned agent {} for task '{}'", agent_id, task.title);

                                // Track in status
                                {
                                    let mut s = status.write().await;
                                    s.active_agents.push(ActiveAgent {
                                        agent_id,
                                        task_id: task.task_id,
                                        task_title: task.title.clone(),
                                        spawned_at: Utc::now().to_rfc3339(),
                                    });
                                }

                                // Monitor completion in background
                                let db2 = db.clone();
                                let status2 = status.clone();
                                let task_title = task.title.clone();
                                let task_id = task.task_id;
                                tokio::spawn(async move {
                                    monitor_agent_completion(db2, agent_id, task_id, &task_title, status2).await;
                                });
                            }
                            Err(e) => {
                                warn!("Engineer: failed to spawn agent for task '{}': {}", task.title, e);
                                // Release task back to PENDING so it can be retried
                                let _ = release_task_to_pending(&db, task.task_id).await;
                            }
                        }
                    } else {
                        info!("Engineer: task '{}' already claimed by another agent", task.title);
                    }
                }
                Ok(None) => {
                    // No pending tasks — idle
                    let s = status.read().await;
                    if s.active_agents.is_empty() {
                        // Fully idle
                    }
                }
                Err(e) => {
                    warn!("Engineer: error querying task queue: {}", e);
                }
            }
        } else {
            info!("Engineer: {} agents active (max {}), waiting", active_count, MAX_CONCURRENT);
        }

        // Sync active_agents list from DB (prune completed ones)
        sync_active_agents(&db, engineer_pid, &status).await;

        sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
    }

    {
        let mut s = status.write().await;
        s.running = false;
    }
    info!("Engineer loop exited");
}

/// Ensure the Engineer persistent agent record exists in frank_persistent_agents
async fn ensure_engineer_exists(db: &PgPool) -> Result<Uuid> {
    // Try to load existing
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM frank_persistent_agents WHERE name = $1 LIMIT 1"
    )
    .bind(ENGINEER_NAME)
    .fetch_optional(db)
    .await?;

    if let Some(pid) = existing {
        return Ok(pid);
    }

    // Create it
    let pid: Uuid = sqlx::query_scalar(
        r#"INSERT INTO frank_persistent_agents (name, role, memory_ns, status, system_prompt)
           VALUES ($1, 'technical_specialist', 'engineer_context', 'idle', $2) RETURNING id"#
    )
    .bind(ENGINEER_NAME)
    .bind(engineer_system_prompt())
    .fetch_one(db)
    .await?;

    info!("Engineer: created persistent agent record {}", pid);
    Ok(pid)
}

/// Count active agents currently running under Engineer's persistent_agent_id
async fn count_active_agents(db: &PgPool, engineer_pid: Uuid) -> Result<usize> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM frankos_agents WHERE persistent_agent_id = $1 AND status IN ('spawned', 'running')"
    )
    .bind(engineer_pid)
    .fetch_one(db)
    .await?;
    Ok(count as usize)
}

/// Find the highest-priority PENDING task (with satisfied dependencies)
async fn find_next_pending_task(db: &PgPool) -> Result<Option<TaskRecord>> {
    use sqlx::Row;

    let rows = sqlx::query(
        r#"SELECT task_id, title, description, priority, assigned_to, dependencies, tags, context
           FROM frank_tasks
           WHERE status = 'PENDING'
           ORDER BY priority DESC, created_at ASC
           LIMIT 20"#
    )
    .fetch_all(db)
    .await?;

    for row in rows {
        let task_id: Uuid = row.try_get("task_id")?;
        let deps: Vec<Uuid> = row.try_get("dependencies").unwrap_or_default();

        // Check dependency satisfaction
        if !deps.is_empty() {
            let incomplete: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM frank_tasks WHERE task_id = ANY($1) AND status != 'COMPLETE'"
            )
            .bind(&deps)
            .fetch_one(db)
            .await
            .unwrap_or(1);

            if incomplete > 0 {
                continue; // Deps not met — skip
            }
        }

        return Ok(Some(TaskRecord {
            task_id,
            title: row.try_get("title")?,
            description: row.try_get("description").ok(),
            priority: row.try_get("priority")?,
            assigned_to: row.try_get("assigned_to").ok(),
            tags: row.try_get("tags").unwrap_or_default(),
            context: row.try_get("context").ok(),
        }));
    }

    Ok(None)
}

/// Atomically claim a task — returns true if claimed, false if already taken
async fn claim_task(db: &PgPool, task_id: Uuid, engineer_pid: Uuid) -> Result<bool> {
    let rows = sqlx::query(
        r#"UPDATE frank_tasks
           SET status = 'IN_PROGRESS', started_at = NOW(), updated_at = NOW(),
               assigned_to = $2
           WHERE task_id = $1 AND status = 'PENDING'
           RETURNING task_id"#
    )
    .bind(task_id)
    .bind(engineer_pid.to_string())
    .fetch_all(db)
    .await?;

    Ok(!rows.is_empty())
}

/// Release a task back to PENDING (e.g. if agent spawn fails)
async fn release_task_to_pending(db: &PgPool, task_id: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE frank_tasks SET status = 'PENDING', started_at = NULL, updated_at = NOW(), assigned_to = NULL WHERE task_id = $1"
    )
    .bind(task_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Spawn a frankos_agent to execute a task, linked to Engineer's persistent_agent_id
async fn spawn_task_agent(
    db: &PgPool,
    task: &TaskRecord,
    engineer_pid: Uuid,
    _brave_key: Option<String>,
) -> Result<Uuid> {
    // Build the goal prompt from task details
    let mut goal = format!(
        "You are working on task: \"{}\"\n\n",
        task.title
    );

    if let Some(desc) = &task.description {
        goal.push_str(&format!("Description: {}\n\n", desc));
    }

    if let Some(ctx) = &task.context {
        goal.push_str(&format!("Context: {}\n\n", serde_json::to_string_pretty(ctx).unwrap_or_default()));
    }

    goal.push_str(
        "Use your available tools to complete this task. \
         When done, call task_done with the task_id and a clear outcome summary. \
         If blocked, call task_block with the reason so Mac Frank can help. \
         Do not stop until the task is complete or blocked."
    );

    // Add task_id to goal so agent can call task_done/task_block
    goal.push_str(&format!("\n\nTask ID: {}", task.task_id));

    // Determine model — high priority tasks get Sonnet, routine get Haiku
    let model = if task.priority >= 8 { "sonnet" } else { "haiku" };

    let agent_id: Uuid = sqlx::query_scalar(
        r#"INSERT INTO frankos_agents
           (name, goal, status, tools_allowed, model, persistent_agent_id, user_id)
           VALUES ($1, $2, 'spawned', $3, $4, $5, $6)
           RETURNING id"#
    )
    .bind(format!("Engineer::{}", task.title.chars().take(30).collect::<String>()))
    .bind(&goal)
    .bind(json!(engineer_tool_allowlist()))
    .bind(model)
    .bind(engineer_pid)
    .bind(Option::<Uuid>::None) // No user context — system-originated
    .fetch_one(db)
    .await?;

    Ok(agent_id)
}

/// Monitor a spawned agent until it completes or times out, then update task status
async fn monitor_agent_completion(
    db: PgPool,
    agent_id: Uuid,
    task_id: Uuid,
    task_title: &str,
    status: SharedEngineerStatus,
) {
    let deadline = Instant::now() + Duration::from_secs(AGENT_TIMEOUT_SECS);

    loop {
        if Instant::now() > deadline {
            warn!("Engineer: agent {} timed out for task '{}'", agent_id, task_title);

            // Mark task blocked
            let msg = format!("Agent {} timed out after {}s", agent_id, AGENT_TIMEOUT_SECS);
            let _ = mark_task_blocked(&db, task_id, &msg).await;
            let _ = write_frank_to_mac(&msg, task_id).await;

            // Update counters
            {
                let mut s = status.write().await;
                s.tasks_blocked_this_session += 1;
                s.active_agents.retain(|a| a.agent_id != agent_id);
            }
            return;
        }

        sleep(Duration::from_secs(10)).await;

        // Check agent status
        match get_agent_status(&db, agent_id).await {
            Ok(agent_status) => {
                match agent_status.as_str() {
                    "complete" => {
                        info!("Engineer: agent {} completed task '{}'", agent_id, task_title);

                        // The agent should have called task_done already.
                        // If not, force-complete it here.
                        let task_status = get_task_status(&db, task_id).await.unwrap_or_default();
                        if task_status != "COMPLETE" && task_status != "BLOCKED" {
                            info!("Engineer: agent completed but task still {}; forcing COMPLETE", task_status);
                            let _ = mark_task_complete(&db, task_id, "Agent completed (auto-close)").await;
                        }

                        {
                            let mut s = status.write().await;
                            s.tasks_completed_this_session += 1;
                            s.active_agents.retain(|a| a.agent_id != agent_id);
                            s.current_task_id = None;
                            s.current_task_title = None;
                        }
                        return;
                    }
                    "failed" => {
                        warn!("Engineer: agent {} failed for task '{}'", agent_id, task_title);

                        let reason = get_agent_error(&db, agent_id).await
                            .unwrap_or_else(|_| "Unknown agent error".to_string());
                        let _ = mark_task_blocked(&db, task_id, &reason).await;
                        let _ = write_frank_to_mac(&reason, task_id).await;

                        {
                            let mut s = status.write().await;
                            s.tasks_blocked_this_session += 1;
                            s.active_agents.retain(|a| a.agent_id != agent_id);
                        }
                        return;
                    }
                    _ => {
                        // Still running — keep waiting
                    }
                }
            }
            Err(e) => {
                warn!("Engineer: error checking agent status: {}", e);
            }
        }
    }
}

/// Sync active_agents list against DB reality (prune completed ones)
async fn sync_active_agents(db: &PgPool, engineer_pid: Uuid, status: &SharedEngineerStatus) {
    let active_ids: Vec<Uuid> = {
        let s = status.read().await;
        s.active_agents.iter().map(|a| a.agent_id).collect()
    };

    if active_ids.is_empty() {
        return;
    }

    // Check which are still running
    let still_running: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM frankos_agents WHERE id = ANY($1) AND status IN ('spawned', 'running')"
    )
    .bind(&active_ids)
    .fetch_all(db)
    .await
    .unwrap_or_default();

    let _ = engineer_pid; // suppress unused warning

    // Prune finished ones from status list
    {
        let mut s = status.write().await;
        s.active_agents.retain(|a| still_running.contains(&a.agent_id));
    }
}

// ── DB helpers ────────────────────────────────────────────────────────────────

async fn get_agent_status(db: &PgPool, agent_id: Uuid) -> Result<String> {
    let status: String = sqlx::query_scalar(
        "SELECT status FROM frankos_agents WHERE id = $1"
    )
    .bind(agent_id)
    .fetch_one(db)
    .await?;
    Ok(status)
}

async fn get_agent_error(db: &PgPool, agent_id: Uuid) -> Result<String> {
    let err: Option<String> = sqlx::query_scalar(
        "SELECT error FROM frankos_agents WHERE id = $1"
    )
    .bind(agent_id)
    .fetch_one(db)
    .await?;
    Ok(err.unwrap_or_else(|| "Unknown error".to_string()))
}

async fn get_task_status(db: &PgPool, task_id: Uuid) -> Result<String> {
    let status: String = sqlx::query_scalar(
        "SELECT status FROM frank_tasks WHERE task_id = $1"
    )
    .bind(task_id)
    .fetch_one(db)
    .await?;
    Ok(status)
}

async fn mark_task_complete(db: &PgPool, task_id: Uuid, outcome: &str) -> Result<()> {
    let ctx_update = json!({ "outcome": outcome, "completed_by": "Engineer::monitor" });
    sqlx::query(
        r#"UPDATE frank_tasks
           SET status = 'COMPLETE', completed_at = NOW(), updated_at = NOW(),
               context = COALESCE(context, '{}'::jsonb) || $2::jsonb
           WHERE task_id = $1"#
    )
    .bind(task_id)
    .bind(ctx_update)
    .execute(db)
    .await?;
    Ok(())
}

async fn mark_task_blocked(db: &PgPool, task_id: Uuid, reason: &str) -> Result<()> {
    let ctx_update = json!({ "blocked_reason": reason, "blocked_by": "Engineer::monitor" });
    sqlx::query(
        r#"UPDATE frank_tasks
           SET status = 'BLOCKED', updated_at = NOW(),
               context = COALESCE(context, '{}'::jsonb) || $2::jsonb
           WHERE task_id = $1"#
    )
    .bind(task_id)
    .bind(ctx_update)
    .execute(db)
    .await?;
    Ok(())
}

async fn write_frank_to_mac(reason: &str, task_id: Uuid) -> Result<()> {
    let now = Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
    let msg = format!(
        "# Engineer → Mac Frank: BLOCKED\n\n**Time:** {}\n**Task:** {}\n**Reason:** {}\n\n**Action needed:** Review blocker and unblock or reassign.\n",
        now, task_id, reason
    );
    tokio::fs::write("/opt/frankos/workspace/COLLAB/FRANK_TO_MAC.md", msg).await?;
    Ok(())
}

// ── Static config ──────────────────────────────────────────────────────────────

/// Tools Engineer sub-agents are allowed to use
fn engineer_tool_allowlist() -> Vec<&'static str> {
    vec![
        // Task management
        "task_list_pending", "task_claim", "task_done", "task_block",
        // Memory
        "memory_write", "memory_search", "memory_semantic_search",
        // Build tools
        "forge_write_file", "forge_read_file", "forge_list_files",
        "process_spawn", "process_wait", "process_status",
        "shell_exec",
        // Agents (for spawning sub-agents)
        "spawn_agent",
        // Skills
        "skill_read", "skill_list",
        // Research
        "brave_search", "web_fetch",
        // Goals/planning visibility
        "goal_list", "goal_get", "plan_list",
    ]
}

/// System prompt for Engineer-spawned sub-agents
fn engineer_system_prompt() -> &'static str {
    "You are Engineer — the autonomous build agent running inside the FrankOS gateway on frank.swarmlogic.cloud.

Your mandate:
- Work through frank_tasks autonomously
- Plan, build, and verify — do not stop until each task is COMPLETE or BLOCKED
- Build knowledge: use memory_write(bucket=build_state) to record decisions
- Recall knowledge: call memory_semantic_search before starting any task
- Always call task_done when finished, task_block when genuinely stuck
- Write blockers to /opt/frankos/workspace/COLLAB/FRANK_TO_MAC.md

Build environment:
- Rust source: /opt/frankos/runtime/frankos-gateway/src/
- Cargo: RUSTUP_HOME=/root/.rustup CARGO_HOME=/root/.cargo /root/.cargo/bin/cargo build --release
- Deploy: /opt/frankos/bin/deploy.sh <label>
- DB: sudo -u postgres psql -d frankos
- COLLAB: /opt/frankos/workspace/COLLAB/

You are always-on. You are not a demo. Do the work."
}

/// Task record for internal use
#[derive(Debug, Clone)]
struct TaskRecord {
    pub task_id: Uuid,
    pub title: String,
    pub description: Option<String>,
    pub priority: i32,
    pub assigned_to: Option<String>,
    pub tags: Vec<String>,
    pub context: Option<Value>,
}
