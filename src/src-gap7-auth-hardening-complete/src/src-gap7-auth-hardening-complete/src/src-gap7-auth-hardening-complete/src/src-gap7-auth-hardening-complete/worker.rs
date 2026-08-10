//! FrankOS Agent Worker — background task that picks up spawned agents and runs them

use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};
use uuid::Uuid;

use crate::llm::LlmClient;

/// Background worker loop — polls for spawned agents every 5 seconds and executes them.
/// This is the missing link: agents are inserted with status='spawned' but only get
/// picked up by spawn_pending_agents (which looks for 'pending') during chat turns.
/// This worker ensures they always get executed regardless of chat activity.
pub async fn run_agent_worker(pool: PgPool, llm: Arc<LlmClient>, brave_key: Option<String>) {
    info!("Agent worker loop started — polling every 5s for spawned agents");

    loop {
        match pick_up_spawned_agents(&pool, llm.clone(), brave_key.clone()).await {
            Ok(count) if count > 0 => {
                info!("Agent worker: dispatched {} agent(s)", count);
            }
            Ok(_) => {}
            Err(e) => {
                warn!("Agent worker: error querying spawned agents: {}", e);
            }
        }

        sleep(Duration::from_secs(5)).await;
    }
}

/// Query for agents with status='spawned', dispatch each into a background tokio task.
async fn pick_up_spawned_agents(
    pool: &PgPool,
    llm: Arc<LlmClient>,
    brave_key: Option<String>,
) -> Result<usize, sqlx::Error> {
    use sqlx::Row;

    let rows = sqlx::query(
        "SELECT id FROM frankos_agents WHERE status = 'spawned' ORDER BY created_at ASC LIMIT 5"
    )
    .fetch_all(pool)
    .await?;

    let count = rows.len();

    for row in rows {
        let agent_id: Uuid = row.try_get("id")?;
        let pool2 = pool.clone();
        let llm2 = llm.clone();
        let key2 = brave_key.clone();

        tokio::spawn(async move {
            crate::agents::run_agent(pool2, llm2, agent_id, key2).await;
        });
    }

    Ok(count)
}
