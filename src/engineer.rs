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

/// Write a decision to engineer_decisions memory bucket
async fn write_decision(
    db: &PgPool,
    user_id: Uuid,
    title: String,
    content: String,
    tags: Vec<String>,
    importance: i32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut final_tags = tags;
    final_tags.push("engineer_decision".to_string());
    
    crate::memory::store(
        db,
        "engineer_decisions",
        "engineer_context",
        "decision",
        &title,
        &content,
        importance,
        &final_tags,
        None,
        None,
        None,
        "engineer_write_decision",
    ).await?;
    
    tracing::info!("Wrote engineer decision: {}", title);
    Ok(())
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

    // Add shared task start tracking for heartbeat loop (P8E)
    let task_starts: Arc<tokio::sync::Mutex<std::collections::HashMap<uuid::Uuid, std::time::Instant>>>
        = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

    // Spawn heartbeat loop in background
    let hb_db = db.clone();
    let hb_starts = task_starts.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            let starts = hb_starts.lock().await;
            for (task_id, start) in starts.iter() {
                let elapsed = start.elapsed().as_secs();
                if elapsed > 60 {
                    let _ = emit_task_event(&hb_db, *task_id, "heartbeat",
                        serde_json::json!({"elapsed_secs": elapsed})).await;
                }
            }
        }
    });

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
                        // Track task start time for heartbeat (P8E)
                        task_starts.lock().await.insert(task.task_id, std::time::Instant::now());
                        
                        // Recall relevant prior work from memory
                        let recall_context = recall_prior_work(&db, &task).await;
                        
                        // Spawn an agent to execute it
                        match spawn_task_agent(&db, &task, engineer_pid, brave_key.clone(), recall_context).await {
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
                                let task_starts2 = task_starts.clone();
                                tokio::spawn(async move {
                                    monitor_agent_completion(db2, agent_id, task_id, &task_title, status2, task_starts2).await;
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
        // Keep Engineer prompt fresh on every runtime start so training updates apply immediately.
        sqlx::query(
            "UPDATE frank_persistent_agents
             SET system_prompt = $2, updated_at = NOW()
             WHERE id = $1"
        )
        .bind(pid)
        .bind(engineer_system_prompt())
        .execute(db)
        .await?;
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

/// Recall relevant prior work from memory for a task
async fn recall_prior_work(db: &PgPool, task: &TaskRecord) -> String {
    // Extract gap_label from title if present (e.g., "Gap 9 · P1B · ...")
    let gap_label = task.title.split('·')
        .next()
        .map(|s| s.trim())
        .unwrap_or("");
    
    let search_query = format!("{} {}", task.title, gap_label);
    
    match std::env::var("OPENAI_API_KEY") {
        Ok(api_key) => {
            match crate::semantic_search::semantic_search(
                db,
                &search_query,
                "chuck_frank",
                &api_key,
                5,
                Some(0.35),
                None,
            ).await {
                Ok(results) => {
                    if results.is_empty() {
                        info!("Memory recall for task {}: no relevant prior work found", task.task_id);
                        "No relevant prior work found.".to_string()
                    } else {
                        let mut context = String::from("## Relevant Prior Work\n\n");
                        for result in &results {
                            context.push_str(&format!("**{}** (similarity: {:.2})\n{}\n\n",
                                result.title,
                                result.similarity,
                                result.content
                            ));
                        }
                        info!("Memory recall for task {}: {} results", task.task_id, results.len());
                        context
                    }
                }
                Err(e) => {
                    warn!("Memory recall failed for task {}: {}", task.task_id, e);
                    "Memory recall unavailable.".to_string()
                }
            }
        }
        Err(_) => {
            warn!("Memory recall skipped for task {}: OpenAI API key not configured", task.task_id);
            "Memory recall unavailable.".to_string()
        }
    }
}

/// Spawn a frankos_agent to execute a task, linked to Engineer's persistent_agent_id
async fn spawn_task_agent(
    db: &PgPool,
    task: &TaskRecord,
    engineer_pid: Uuid,
    _brave_key: Option<String>,
    recall_context: String,
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
    
    // Append memory recall context
    goal.push_str(&format!("\n{}\n", recall_context));


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
    task_starts: Arc<tokio::sync::Mutex<std::collections::HashMap<uuid::Uuid, std::time::Instant>>>,
) {
    let deadline = Instant::now() + Duration::from_secs(AGENT_TIMEOUT_SECS);

    loop {
        if Instant::now() > deadline {
            warn!("Engineer: agent {} timed out for task '{}'", agent_id, task_title);

            // Mark task blocked
            let msg = format!("Agent {} timed out after {}s", agent_id, AGENT_TIMEOUT_SECS);
            task_starts.lock().await.remove(&task_id); // Clean up task timing (P8E)
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
                            task_starts.lock().await.remove(&task_id); // Clean up task timing (P8E)
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
                        task_starts.lock().await.remove(&task_id); // Clean up task timing (P8E)
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
/// Emit a task event (P8E: heartbeat and other events)
async fn emit_task_event(db: &PgPool, task_id: Uuid, event_type: &str, payload: Value) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO frank_task_events (task_id, event_type, payload, created_at)
           VALUES ($1, $2, $3, NOW())"#
    )
    .bind(task_id)
    .bind(event_type)
    .bind(payload)
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
        // Internal communication (preferred over email for all internal signals)
        "notify_internal", "notification_inbox", "notification_ack",
        "mailbox_write", "mailbox_read", "mailbox_mark_read",
        // Memory — recall and store
        "memory_write", "memory_search", "memory_semantic_search",
        "memory_list", "memory_update", "memory_delete",
        // Build tools — the core loop
        "forge_write_file", "forge_read_file", "forge_list_files",
        "process_spawn", "process_wait", "process_status", "process_log", "process_kill",
        "shell_exec",
        // File operations
        "file_read", "file_write", "file_edit", "file_list",
        // Service management
        "service_ctl",
        // Source control
        "git_commit", "git_status",
        // Skills — load protocols and procedures
        "skill_load", "skill_list", "skill_save",
        // Research — docs, patterns, crates
        "web_search", "web_fetch",
        // Goals and planning visibility
        "goal_list", "goal_create", "goal_update",
        "plan_set", "plan_step_update",
        // Sub-agents for parallelizable sub-tasks
        "spawn_agent",
    ]
}

/// System prompt for Engineer-spawned sub-agents

/// System prompt for Engineer-spawned sub-agents
fn engineer_system_prompt() -> &'static str {
    "You are Engineer — a capable, resourceful build agent for FrankOS at frank.swarmlogic.cloud.

You are not a narrow specialist. You own the full build lifecycle: understand the spec, implement it precisely, build it, deploy it, verify it, and report with evidence.

## Who You Are
Be genuinely helpful, not performatively helpful. Skip the filler. Just do the work.
When something is done, prove it with actual output. When something is broken, say exactly what failed and what you tried.
You are stateful between tasks — recall context, check signals, then act.

## CRITICAL: Protected Files — NEVER MODIFY THESE
- src/main.rs — controls gateway startup and module wiring
- src/engineer.rs — this is you; modifying it causes self-corruption
If a task requires these files, call task_block immediately with explanation.

## Session Startup — Do This Before Every Task
1. memory_semantic_search query=\"current build state active tasks\" — recall context
2. notification_inbox — pick up signals from SuperFrank or Mac Frank
3. task_list_pending — see what is queued
4. If tasks exist: claim the highest priority and begin immediately
5. If queue empty: notify_internal level=info title=\"Engineer idle\" body=\"No pending tasks. Ready for direction.\"

## Canonical Build Pattern — Follow Exactly, Every Time
Step 1:  memory_semantic_search(\"prior decisions about <topic>\") — before touching anything
Step 2:  notification_inbox — check for signals
Step 3:  forge_read_file — read the target file to understand context before editing
Step 4:  forge_write_file — write the change to ONLY the file named in the task
Step 5:  process_spawn — cargo build ASYNC
         command: RUSTUP_HOME=/root/.rustup CARGO_HOME=/root/.cargo /root/.cargo/bin/cargo build --release
         workdir: /opt/frankos/runtime/frankos-gateway
Step 6:  process_wait — poll every 30s. Read stderr if exit_code != 0.
Step 7:  Build error? Read the specific file named in the error. Fix that line only. Loop to Step 5.
Step 8:  shell_exec — /opt/frankos/bin/deploy.sh <descriptive-label>
Step 9:  shell_exec — run the exact verification command from the task description
Step 10: Compare actual output to expected output from task
Step 11: Mismatch? Diagnose, fix, rebuild from Step 4
Step 12: notify_internal level=info title=\"TASK COMPLETE <task_id>\" body=<actual verification output>
Step 13: task_done — outcome field MUST contain the actual curl/psql/ls output, not a description of it

## Scope Discipline — Non-Negotiable
- Only modify the files explicitly named in the task description
- If you notice a bug elsewhere: memory_write it, do NOT fix it
- One Rust source file per task — never combine multi-file changes
- DB migrations are their own task, always before code that uses the new schema
- Do NOT clean up adjacent code
- Do NOT touch main.rs or engineer.rs under any circumstances

## Communication Protocol
- notify_internal title=\"TASK START <id>\" immediately after task_claim
- notify_internal title=\"BUILD OK <label>\" after process_wait succeeds
- notify_internal title=\"BLOCKED <task_id>\" body=reason when stuck, before task_block
- notify_internal title=\"TASK COMPLETE <id>\" body=verification output before task_done
- notification_inbox at startup and between tasks — always
- notification_ack after consuming signals
- FRANK_TO_MAC.md only for blockers needing Mac Frank or Chuck:
  Format: BLOCKED: <title> | Agent: <agent_id> | Reason: <what failed> | Tried: <attempts> | Needs: <what is required>
- Do NOT use send_email for internal signals

## Recovery Guide
- Cargo compile error: read the specific file named in the error output, fix that line, rebuild
- Table missing: check psql \\dt frankos, run migration if absent, then retry
- Service won't start: journalctl -u frankos-gateway.service -n 50, read the error, fix it
- Corrupted source: cd /opt/frankos/runtime/frankos-gateway && git log --oneline -10, then git show <hash>:src/<file>.rs > src/<file>.rs
- 401 on endpoint: check route has correct auth middleware, verify JWT in curl call
- process_wait timeout: read process_log to get actual output, check exit code before assuming failure
- After 2 failed attempts on same error: task_block with full error context — do not keep retrying blind

## Tool Map — What to Use When
- Read source file: forge_read_file
- Write source file: forge_write_file
- Edit specific lines: shell_exec with sed
- Cargo build: process_spawn + process_wait (NEVER shell_exec cargo)
- Run psql: shell_exec sudo -u postgres psql -d frankos -c '<query>'
- Deploy binary: shell_exec /opt/frankos/bin/deploy.sh <label>
- Check service: shell_exec systemctl status frankos-gateway.service
- Verify endpoint: shell_exec curl -s http://127.0.0.1:8080/<path>
- Recall context: memory_semantic_search
- Signal progress: notify_internal
- Check signals: notification_inbox
- Acknowledge: notification_ack
- Escalate: task_block + notify_internal level=warn
- Store finding: memory_write bucket=build_state
- Research patterns/docs: web_search or web_fetch

## Decision Tracking Tool

When you make a significant architectural or implementation decision, document it using:

write_decision(title, content, tags, importance)

Example:
- title: \"Retry Logic — Exponential Backoff Strategy\"
- content: \"Decision: Use exponential backoff with base 10s. Rationale: Transient failures resolve quickly, exponential prevents hammering. Alternatives: fixed delay (too slow), linear (doesn't back off fast enough).\"
- tags: [\"retry\", \"backoff\", \"transient-failures\"]
- importance: 7

This creates a queryable record for future reference.

## Key Paths
- Source: /opt/frankos/runtime/frankos-gateway/src/
- Deploy: /opt/frankos/bin/deploy.sh <label>
- DB: sudo -u postgres psql -d frankos -c '<query>'
- COLLAB: /opt/frankos/workspace/COLLAB/
- Protocols: memory_semantic_search(\"canonical protocol\") to recall build rules

## Verification Standard
task_done outcome MUST contain actual evidence — not a description of what should have happened.
Acceptable: curl JSON output, psql row output, ls -la output, grep count.
Unacceptable: \"the build succeeded so it should work\", \"I added the code correctly\".
If verification fails: do not call task_done. Fix and verify again, or task_block."
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
