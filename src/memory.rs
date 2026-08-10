//! Memory system — scoped recall pipeline

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: Uuid,
    pub bucket: String,
    pub namespace: String,
    pub memory_type: String,
    pub title: String,
    pub content: String,
    pub importance: i32,
    pub tags: Vec<String>,
}

#[derive(Debug, Default)]
pub struct RecallContext {
    pub telos: Vec<MemoryEntry>,
    pub character: Vec<MemoryEntry>,
    pub work: Vec<MemoryEntry>,
    pub project: Vec<MemoryEntry>,
    pub build_state: Vec<MemoryEntry>,
}

impl RecallContext {
    pub fn to_context_block(&self) -> String {
        let mut parts = vec![];
        if !self.telos.is_empty() {
            parts.push("## Identity & Purpose".to_string());
            for m in &self.telos {
                parts.push(format!("**{}**: {}", m.title, m.content));
            }
        }
        if !self.character.is_empty() {
            parts.push("\n## Relationship & Preferences".to_string());
            for m in &self.character {
                parts.push(format!("- {}: {}", m.title, m.content));
            }
        }
        if !self.work.is_empty() {
            parts.push("\n## Active Work Context".to_string());
            for m in &self.work {
                let snip_end = m.content.char_indices().map(|(i, _)| i).take_while(|&i| i <= 200).last().unwrap_or(0);
                let snip = &m.content[..snip_end];
                parts.push(format!("- [{}] {}: {}", m.memory_type, m.title, snip));
            }
        }
        if !self.project.is_empty() {
            parts.push("\n## Project Memory".to_string());
            for m in &self.project {
                let snip_end = m.content.char_indices().map(|(i, _)| i).take_while(|&i| i <= 200).last().unwrap_or(0);
                let snip = &m.content[..snip_end];
                parts.push(format!("- {}: {}", m.title, snip));
            }
        }
        if !self.build_state.is_empty() {
            parts.push("\n## Active Build State".to_string());
            for m in &self.build_state {
                let snip_end = m.content.char_indices().map(|(i, _)| i).take_while(|&i| i <= 150).last().unwrap_or(0);
                let snip = &m.content[..snip_end];
                parts.push(format!("- **{}** [{}]: {}", m.title, m.memory_type, snip));
            }
        }
        parts.join("\n")
    }
}

pub async fn recall(
    pool: &PgPool,
    namespace: &str,
    _project_id: Option<Uuid>,
    limit_per_layer: i64,
) -> Result<RecallContext> {
    let mut ctx = RecallContext::default();

    let telos_rows = sqlx::query(
        "SELECT id, bucket, namespace, memory_type, title, content, importance, tags
         FROM frankos_memory WHERE namespace = $1 AND bucket = 'personal_telos'
         ORDER BY importance DESC LIMIT $2"
    ).bind(namespace).bind(limit_per_layer).fetch_all(pool).await.unwrap_or_default();
    ctx.telos = rows_to_entries(telos_rows);

    let work_rows = sqlx::query(
        "SELECT id, bucket, namespace, memory_type, title, content, importance, tags
         FROM frankos_memory WHERE namespace = $1 AND bucket = 'personal_work'
         ORDER BY importance DESC, updated_at DESC LIMIT $2"
    ).bind(namespace).bind(limit_per_layer).fetch_all(pool).await.unwrap_or_default();
    ctx.work = rows_to_entries(work_rows);

    let build_state_rows = sqlx::query(
        "SELECT id, bucket, namespace, memory_type, title, content, importance, tags
         FROM frankos_memory WHERE namespace = $1 AND bucket = 'build_state' AND is_active = true
         ORDER BY importance DESC, updated_at DESC LIMIT 5"
    ).bind(namespace).fetch_all(pool).await.unwrap_or_default();
    ctx.build_state = rows_to_entries(build_state_rows);

    // Inject RUNBOOK.md as a high-priority build_state entry if it exists
    if let Ok(runbook) = tokio::fs::read_to_string("/opt/frankos/workspace/RUNBOOK.md").await {
        let runbook_trimmed = if runbook.len() > 6000 {
            runbook[..6000].to_string() + "\n...[RUNBOOK truncated — read full file with file_read /opt/frankos/workspace/RUNBOOK.md]"
        } else {
            runbook
        };
        ctx.build_state.insert(0, MemoryEntry {
            id: uuid::Uuid::nil(),
            bucket: "build_state".to_string(),
            namespace: namespace.to_string(),
            memory_type: "runbook".to_string(),
            title: "RUNBOOK — Operational Reference".to_string(),
            content: runbook_trimmed,
            importance: 10,
            tags: vec!["runbook".to_string(), "operational".to_string()],
        });
    }

    Ok(ctx)
}

pub async fn store(
    pool: &PgPool,
    bucket: &str,
    namespace: &str,
    memory_type: &str,
    title: &str,
    content: &str,
    importance: i32,
    tags: &[String],
    _project_id: Option<Uuid>,
    _organization_id: Option<Uuid>,
    session_id: Option<Uuid>,
    source: &str,
) -> Result<Uuid> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO frankos_memory (bucket, namespace, memory_type, title, content, importance, tags, session_id, source)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING id"
    )
    .bind(bucket).bind(namespace).bind(memory_type)
    .bind(title).bind(content).bind(importance)
    .bind(tags).bind(session_id).bind(source)
    .fetch_one(pool).await?;

    // Emit memory_write success event (Gap 8A instrumentation)
    let pool_event = pool.clone();
    let title_event = title.to_string();
    let mem_type_event = memory_type.to_string();
    let id_event = id;
    tokio::spawn(async move {
        crate::system_events::emit_event(
            &pool_event,
            crate::system_events::event_type::MEMORY_WRITE,
            crate::system_events::severity::INFO,
            serde_json::json!({
                "memory_id": id_event.to_string(),
                "title": title_event,
                "memory_type": mem_type_event,
                "success": true,
            }),
        ).await;
    });

    // Generate embedding asynchronously (fire and forget)
    let pool_clone = pool.clone();
    let pool_embed_event = pool.clone();
    let id_clone = id;
    let title_clone = title.to_string();
    let content_clone = content.to_string();
    tokio::spawn(async move {
        if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
            let text = format!("{}\n{}", title_clone, content_clone);
            if let Err(e) = crate::semantic_search::update_memory_embedding(&pool_clone, &id_clone.to_string(), &text, &api_key).await {
                let err_str = format!("{:#}", e);
                eprintln!("Failed to generate embedding for memory {}: {}", id_clone, err_str);
                // Emit embedding failure event (Gap 8A)
                crate::system_events::emit_event(
                    &pool_embed_event,
                    crate::system_events::event_type::TOOL_FAILURE,
                    crate::system_events::severity::ERROR,
                    serde_json::json!({
                        "tool": "embedding_generate",
                        "memory_id": id_clone.to_string(),
                        "error": err_str,
                    }),
                ).await;
            }
        } else {
            tracing::warn!("OPENAI_API_KEY not set — skipping embedding for memory {}", id_clone);
        }
    });

    Ok(id)
}

fn rows_to_entries(rows: Vec<sqlx::postgres::PgRow>) -> Vec<MemoryEntry> {
    use sqlx::Row;
    rows.iter().filter_map(|r| {
        Some(MemoryEntry {
            id: r.try_get("id").ok()?,
            bucket: r.try_get("bucket").unwrap_or_default(),
            namespace: r.try_get("namespace").unwrap_or_default(),
            memory_type: r.try_get("memory_type").unwrap_or_default(),
            title: r.try_get("title").unwrap_or_default(),
            content: r.try_get("content").unwrap_or_default(),
            importance: r.try_get("importance").unwrap_or(5),
            tags: r.try_get("tags").unwrap_or_default(),
        })
    }).collect()
}
