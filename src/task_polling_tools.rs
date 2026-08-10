//! Task Polling Integration Tools
//! 
//! Provides tools for agents to poll and execute tasks autonomously.
//! Integrates the task_polling module with the agent tool system.

use crate::task_polling;
use serde_json::{json, Value};
use sqlx::PgPool;
use tracing::info;

/// Tool output structure
#[derive(Debug)]
pub struct ToolOutput {
    pub success: bool,
    pub output: Value,
}

/// Execute autonomous task polling
pub async fn poll_tasks(
    db: &PgPool,
    agent_name: &str,
    _input: Value,
) -> ToolOutput {
    info!("Agent {} initiating task polling loop", agent_name);
    
    match task_polling::poll_tasks_autonomous(db, agent_name).await {
        Ok(results) => {
            let summary = if results.is_empty() {
                "No eligible tasks found".to_string()
            } else {
                format!(
                    "Processed {} tasks: {} completed, {} blocked",
                    results.len(),
                    results.iter().filter(|r| r.status == "COMPLETE").count(),
                    results.iter().filter(|r| r.status == "BLOCKED").count(),
                )
            };
            
            ToolOutput {
                success: true,
                output: json!({
                    "summary": summary,
                    "tasks_processed": results.len(),
                    "results": results.iter().map(|r| json!({
                        "task_id": r.task_id,
                        "title": r.title,
                        "status": r.status,
                        "outcome": r.outcome,
                    })).collect::<Vec<_>>(),
                }),
            }
        }
        Err(e) => {
            ToolOutput {
                success: false,
                output: json!({
                    "error": format!("Task polling failed: {}", e),
                    "details": e.to_string(),
                }),
            }
        }
    }
}

/// Get task polling statistics
pub async fn get_task_stats(
    db: &PgPool,
    _agent_name: &str,
    _input: Value,
) -> ToolOutput {
    match task_polling::get_polling_stats(db).await {
        Ok(stats) => {
            ToolOutput {
                success: true,
                output: json!({
                    "stats": stats,
                }),
            }
        }
        Err(e) => {
            ToolOutput {
                success: false,
                output: json!({
                    "error": format!("Failed to get stats: {}", e),
                }),
            }
        }
    }
}
