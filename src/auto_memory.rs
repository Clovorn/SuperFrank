//! Gap 7D — Auto Memory Write
//! Automatically capture significant actions to memory

use sqlx::PgPool;
use tracing::info;
use uuid::Uuid;

/// Hook: Auto-write memory after cargo_build success
pub async fn after_cargo_build(
    db: &PgPool,
    path: &str,
    success: bool,
    goal_id: Option<Uuid>,
) {
    if !success {
        return; // Only record successful builds
    }

    let project = path.split('/').last().unwrap_or("unknown");
    let title = format!("Build: {}", project);
    let content = format!(
        "Successfully built {} at {} on {}",
        project,
        path,
        chrono::Utc::now().format("%Y-%m-%d %H:%M UTC")
    );

    let mut tags = vec!["build".to_string(), "cargo".to_string(), project.to_string()];
    if let Some(gid) = goal_id {
        tags.push(format!("goal:{}", gid));
    }

    let _ = write_memory(db, &title, &content, "procedure", tags, 4, goal_id).await;
}

/// Hook: Auto-write memory after service restart
pub async fn after_service_restart(
    db: &PgPool,
    service: &str,
    success: bool,
    goal_id: Option<Uuid>,
) {
    if !success {
        return;
    }

    let title = format!("Deploy: {}", service);
    let content = format!(
        "Deployed {} via systemctl restart on {}",
        service,
        chrono::Utc::now().format("%Y-%m-%d %H:%M UTC")
    );

    let mut tags = vec!["deploy".to_string(), service.to_string()];
    if let Some(gid) = goal_id {
        tags.push(format!("goal:{}", gid));
    }

    let _ = write_memory(db, &title, &content, "procedure", tags, 5, goal_id).await;
}

/// Hook: Auto-write memory after file_write to architecture/spec files
pub async fn after_architecture_write(
    db: &PgPool,
    path: &str,
    goal_id: Option<Uuid>,
) {
    // Only track writes to architecture-significant files
    let arch_patterns = ["/src/", ".md", "Cargo.toml", ".json", ".yaml", ".sql"];
    if !arch_patterns.iter().any(|p| path.contains(p)) {
        return;
    }

    let filename = path.split('/').last().unwrap_or("file");
    let title = format!("Architecture: {}", filename);
    let content = format!(
        "Modified {} on {}. File path: {}",
        filename,
        chrono::Utc::now().format("%Y-%m-%d %H:%M UTC"),
        path
    );

    let mut tags = vec!["architecture".to_string(), "file_write".to_string()];
    if let Some(gid) = goal_id {
        tags.push(format!("goal:{}", gid));
    }

    let _ = write_memory(db, &title, &content, "decision", tags, 4, goal_id).await;
}

/// Hook: Auto-write memory after goal completion
pub async fn after_goal_complete(
    db: &PgPool,
    goal_title: &str,
    notes: Option<&str>,
    goal_id: Uuid,
) {
    let title = format!("Completed: {}", goal_title);
    let content = if let Some(n) = notes {
        format!(
            "Goal completed on {}. Notes: {}",
            chrono::Utc::now().format("%Y-%m-%d"),
            n
        )
    } else {
        format!("Goal completed on {}", chrono::Utc::now().format("%Y-%m-%d"))
    };

    let tags = vec!["completion".to_string(), "goal".to_string()];
    let _ = write_memory(db, &title, &content, "lesson", tags, 6, Some(goal_id)).await;
}

/// Hook: Auto-write memory after step completion (significant steps only)
pub async fn after_step_complete(
    db: &PgPool,
    goal_title: &str,
    step_title: &str,
    step_number: i32,
    notes: Option<&str>,
    goal_id: Uuid,
) {
    // Only record steps with notes or every 5th step
    if notes.is_none() && step_number % 5 != 0 {
        return;
    }

    let title = format!("Step {}: {}", step_number, step_title);
    let content = if let Some(n) = notes {
        format!(
            "Completed step {} of '{}' on {}. Notes: {}",
            step_number,
            goal_title,
            chrono::Utc::now().format("%Y-%m-%d"),
            n
        )
    } else {
        format!(
            "Completed step {} of '{}' on {}",
            step_number,
            goal_title,
            chrono::Utc::now().format("%Y-%m-%d")
        )
    };

    let tags = vec![
        "step".to_string(),
        format!("goal:{}", goal_id),
        "progress".to_string(),
    ];
    let _ = write_memory(db, &title, &content, "procedure", tags, 4, Some(goal_id)).await;
}

/// Core memory write helper
/// Gap 8B.5: Now logs failures instead of silently dropping them
async fn write_memory(
    db: &PgPool,
    title: &str,
    content: &str,
    memory_type: &str,
    tags: Vec<String>,
    importance: i32,
    project_id: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    info!("Auto memory write: {} (type: {})", title, memory_type);

    match sqlx::query(
        r#"INSERT INTO frankos_memory 
           (bucket, namespace, title, content, memory_type, tags, importance, project_id)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"#,
    )
    .bind("work")
    .bind("chuck_frank")
    .bind(title)
    .bind(content)
    .bind(memory_type)
    .bind(&tags)
    .bind(importance)
    .bind(project_id)
    .execute(db)
    .await {
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::error!(
                "Auto memory write FAILED for '{}': {}. Content will be lost unless manually recovered.",
                title, e
            );
            // Future: write to fallback file at /opt/frankos/workspace/FAILED_MEMORIES.jsonl
            Err(e)
        }
    }
}
