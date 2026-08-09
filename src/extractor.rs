//! Memory extraction — auto-promote significant conversation moments
//! Gap 7B: Aggressive, type-aware classification. Working noise discarded.

use anyhow::Result;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use crate::llm::{ChatMessage, LlmClient, LlmProvider};
use tracing::info;

/// Gap 7B: Aggressive extraction — classify every turn
/// Decision? Procedure? Lesson? Working noise?
/// Decisions and procedures get memory_write immediately, not kept in conversation.
pub fn should_extract(user_msg: &str, assistant_msg: &str) -> bool {
    let combined = format!("{} {}", user_msg, assistant_msg).to_lowercase();

    // Explicit skip signals — routine status checks, trivial acks
    let skip_signals = ["ok", "done", "got it", "understood", "checking"];
    let is_trivial = skip_signals.iter().any(|s| combined.len() < 50 && combined.contains(s));
    if is_trivial {
        return false;
    }

    // Strong extraction signals — decision, procedure, lesson, architecture
    let strong_signals = [
        // Decision signals
        "decided", "we will", "going to", "the plan is", "confirmed", "agreed",
        "ratified", "from now on", "the rule is", "approved", "final decision",
        
        // Procedure signals
        "the pattern is", "always do", "never do", "standard procedure",
        "build order", "deployment pattern", "the way we", "from now on",
        
        // Lesson signals
        "learned that", "the mistake was", "next time", "the fix was",
        "root cause", "the issue was", "important:", "note:",
        
        // Architecture signals
        "architecture", "design", "structure", "the way", "implementation",
        "module", "service", "endpoint", "database", "schema",
        
        // Telos/character signals
        "ethos", "telos", "purpose", "character", "philosophy",
        "theology", "worldview", "principle", "north star",
    ];

    for sig in &strong_signals {
        if combined.contains(sig) {
            return true;
        }
    }

    // Substantive exchanges with context
    if user_msg.len() > 100 && assistant_msg.len() > 200 {
        return true;
    }

    false
}

/// Ask the LLM to classify and extract memory — Gap 7B strict typing
pub async fn extract_memory(
    llm: &LlmClient,
    user_msg: &str,
    assistant_msg: &str,
) -> Result<Option<ExtractedMemory>> {
    let prompt = format!(
        r#"Classify this conversation exchange. Is it a durable memory or working noise?

USER: {user}

FRANK: {assistant}

Classify into ONE of these types:

1. **decision** — a choice made about architecture, tooling, process, or direction
   Example: "We're using OpenAI embeddings for semantic search"

2. **procedure** — a pattern, rule, or process established for future work
   Example: "Build order: write code, spawn cargo build, deploy, restart service"

3. **lesson** — an error encountered, root cause identified, fix documented
   Example: "Unwrap on non-nullable String caused panic. Use .clone() instead."

4. **concept** — a key insight, principle, or architectural understanding
   Example: "Sessions are ephemeral workbenches. Memory is the source of truth."

5. **working_noise** — routine status, trivial acks, transient working context
   Example: "Starting build now" or "Got it, moving to next step"

If type is decision/procedure/lesson/concept, respond with JSON:
{{
  "worth_storing": true,
  "memory_type": "decision|procedure|lesson|concept",
  "title": "brief title (under 60 chars)",
  "content": "what to remember (2-4 sentences, dense and specific)",
  "importance": 1-10
}}

If type is working_noise, respond with:
{{"worth_storing": false}}

Respond with JSON only. No markdown, no explanation."#,
        user = user_msg,
        assistant = assistant_msg,
    );

    let response = llm.complete(
        &LlmProvider::Anthropic,
        "claude-haiku-4-5",
        "You are a memory classification system. Classify conversation turns into durable types or working noise. Respond with JSON only.",
        &[ChatMessage { role: "user".to_string(), content: prompt }],
        512,
    ).await?;

    // Parse JSON response — strip markdown if present
    let trimmed = response.trim().trim_start_matches("```json").trim_end_matches("```").trim();
    let v: serde_json::Value = serde_json::from_str(trimmed)?;

    if v["worth_storing"].as_bool().unwrap_or(false) {
        Ok(Some(ExtractedMemory {
            memory_type: v["memory_type"].as_str().unwrap_or("concept").to_string(),
            title: v["title"].as_str().unwrap_or("Untitled").to_string(),
            content: v["content"].as_str().unwrap_or("").to_string(),
            importance: v["importance"].as_i64().unwrap_or(5) as i32,
        }))
    } else {
        Ok(None)
    }
}

#[derive(Debug)]
pub struct ExtractedMemory {
    pub memory_type: String,
    pub title: String,
    pub content: String,
    pub importance: i32,
}

/// Full extraction pipeline — call after each assistant response
/// Gap 7B: Decisions and procedures extracted immediately, not kept in conversation
pub async fn maybe_extract_and_store(
    db: &PgPool,
    llm: &LlmClient,
    namespace: &str,
    session_id: Uuid,
    user_msg: &str,
    assistant_msg: &str,
    chat_bucket: &str,
    chat_folder: Option<&str>,
) -> Result<bool> {
    if !should_extract(user_msg, assistant_msg) {
        return Ok(false);
    }

    match extract_memory(llm, user_msg, assistant_msg).await? {
        Some(extracted) => {
            let mem_bucket = match chat_bucket {
                "training" => "training",
                "work"     => "work",
                _          => "personal",
            };
            let tags: Vec<String> = chat_folder.iter().map(|f| f.to_string()).collect();
            info!("Memory extracted: {} ({}) -> {}", extracted.title, extracted.memory_type, mem_bucket);
            // Gap 8A: emit extraction_decision event
            crate::events::emit_extraction_decision(
                db,
                user_msg,
                true,
                Some(&extracted.memory_type),
                Some(&extracted.title),
            ).await;
            crate::memory::store(
                db,
                mem_bucket,
                namespace,
                &extracted.memory_type,
                &extracted.title,
                &extracted.content,
                extracted.importance,
                &tags,
                None,
                None,
                Some(session_id),
                "extracted",
            ).await?;
            Ok(true)
        }
        None => {
            // Gap 8A: emit extraction_decision event for noise (sampled — not every message)
            // Only emit for long exchanges to avoid flooding
            if user_msg.len() > 100 {
                crate::events::emit_extraction_decision(db, user_msg, false, None, None).await;
            }
            Ok(false)
        }
    }
}
