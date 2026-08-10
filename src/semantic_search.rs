use anyhow::{Context, Result};
use sqlx::PgPool;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use crate::embeddings::generate_embedding;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SemanticSearchResult {
    pub id: String,
    pub title: String,
    pub content: String,
    pub memory_type: Option<String>,
    pub namespace: String,
    pub bucket: String,
    pub importance: i32,
    pub tags: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub similarity: f32,
}

/// Perform semantic search across frankos_memory using vector similarity
pub async fn semantic_search(
    pool: &PgPool,
    query: &str,
    namespace: &str,
    api_key: &str,
    limit: i32,
    similarity_threshold: Option<f32>,
) -> Result<Vec<SemanticSearchResult>> {
    // Generate embedding for the query
    let query_embedding = generate_embedding(query, api_key)
        .await
        .context("Failed to generate query embedding")?;

    let threshold = similarity_threshold.unwrap_or(0.3);

    // Convert Vec<f32> to pgvector format string
    let embedding_str = format!("[{}]", query_embedding.iter()
        .map(|f| f.to_string())
        .collect::<Vec<_>>()
        .join(","));

    // Perform vector similarity search using cosine distance
    // pgvector: <=> is cosine distance, <#> is negative inner product, <-> is L2 distance
    // We use 1 - cosine_distance to get similarity score (higher = more similar)
    let results = sqlx::query_as::<_, SemanticSearchResult>(
        r#"
        SELECT 
            id::text as id,
            title,
            content,
            memory_type,
            namespace,
            bucket,
            importance,
            tags,
            created_at,
            updated_at,
            (1 - (embedding <=> $1::text::vector(1536)))::float4 as similarity
        FROM frankos_memory
        WHERE namespace = $2
            AND embedding IS NOT NULL
            AND (1 - (embedding <=> $1::text::vector(1536))) >= $3
        ORDER BY embedding <=> $1::text::vector(1536)
        LIMIT $4
        "#,
    )
    .bind(&embedding_str)
    .bind(namespace)
    .bind(threshold)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("Failed to execute semantic search query")?;

    Ok(results)
}

/// Hybrid search: combine semantic + keyword search
pub async fn hybrid_search(
    pool: &PgPool,
    query: &str,
    namespace: &str,
    api_key: &str,
    limit: i32,
) -> Result<Vec<SemanticSearchResult>> {
    // Generate embedding for semantic search
    let query_embedding = generate_embedding(query, api_key)
        .await
        .context("Failed to generate query embedding")?;

    let embedding_str = format!("[{}]", query_embedding.iter()
        .map(|f| f.to_string())
        .collect::<Vec<_>>()
        .join(","));

    // Hybrid search: combine semantic similarity with keyword matching
    // Score = 0.7 * semantic_similarity + 0.3 * keyword_match
    let results = sqlx::query_as::<_, SemanticSearchResult>(
        r#"
        WITH semantic AS (
            SELECT 
                id,
                title,
                content,
                memory_type,
                namespace,
                bucket,
                importance,
                tags,
                created_at,
                updated_at,
                1 - (embedding <=> $1::vector) as semantic_score
            FROM frankos_memory
            WHERE namespace = $2
                AND embedding IS NOT NULL
        ),
        keyword AS (
            SELECT 
                id,
                CASE 
                    WHEN title ILIKE '%' || $3 || '%' OR content ILIKE '%' || $3 || '%' THEN 1.0
                    ELSE 0.0
                END as keyword_score
            FROM frankos_memory
            WHERE namespace = $2
        )
        SELECT 
            s.id,
            s.title,
            s.content,
            s.memory_type,
            s.namespace,
            s.bucket,
            s.importance,
            s.tags,
            s.created_at,
            s.updated_at,
            (0.7 * s.semantic_score + 0.3 * COALESCE(k.keyword_score, 0.0)) as similarity
        FROM semantic s
        LEFT JOIN keyword k ON s.id = k.id
        WHERE (0.7 * s.semantic_score + 0.3 * COALESCE(k.keyword_score, 0.0)) >= 0.3
        ORDER BY similarity DESC
        LIMIT $4
        "#,
    )
    .bind(&embedding_str)
    .bind(namespace)
    .bind(query)
    .bind(limit)
    .fetch_all(pool)
    .await
    .context("Failed to execute hybrid search query")?;

    Ok(results)
}

/// Update embedding for a memory entry
pub async fn update_memory_embedding(
    pool: &PgPool,
    memory_id: &str,
    text: &str,
    api_key: &str,
) -> Result<()> {
    let embedding = generate_embedding(text, api_key)
        .await
        .context("Failed to generate embedding")?;

    let embedding_str = format!("[{}]", embedding.iter()
        .map(|f| f.to_string())
        .collect::<Vec<_>>()
        .join(","));

    let memory_uuid = uuid::Uuid::parse_str(memory_id)
        .context("Invalid memory_id UUID format")?;

    sqlx::query(
        r#"
        UPDATE frankos_memory
        SET embedding = $1::text::vector, updated_at = NOW()
        WHERE id = $2
        "#,
    )
    .bind(&embedding_str)
    .bind(memory_uuid)
    .execute(pool)
    .await
    .context("Failed to update memory embedding")?;

    Ok(())
}

/// Backfill embeddings for all memories that don't have them yet
pub async fn backfill_embeddings(
    pool: &PgPool,
    api_key: &str,
    batch_size: i64,
) -> Result<usize> {
    let memories = sqlx::query!(
        r#"
        SELECT id, title, content
        FROM frankos_memory
        WHERE embedding IS NULL
        LIMIT $1
        "#,
        batch_size
    )
    .fetch_all(pool)
    .await
    .context("Failed to fetch memories for backfill")?;

    let total = memories.len();

    for memory in memories {
        let text = format!("{}\n{}", memory.title, memory.content);
        let id_str = memory.id.to_string();
        match update_memory_embedding(pool, &id_str, &text, api_key).await {
            Ok(_) => {},
            Err(e) => {
                eprintln!("Failed to generate embedding for memory {}: {}", memory.id, e);
            }
        }
        // Rate limit
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }

    Ok(total)
}
