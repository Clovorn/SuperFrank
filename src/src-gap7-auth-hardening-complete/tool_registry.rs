//! Tool Registry System — Gap 9
//!
//! Manages registration, certification, and health monitoring of tools.
//! Enables Security Agent's toolbox custodian mode.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Row};
use tracing::info;
use uuid::Uuid;

/// Tool spec defines what a tool is and how it can be called
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Human-readable name
    pub name: String,
    /// What this tool does
    pub purpose: String,
    /// Input schema (JSON schema)
    pub inputs: Value,
    /// Output schema (JSON schema)
    pub outputs: Value,
    /// Required permissions (e.g., ["shell_exec", "file_read"])
    pub permissions: Vec<String>,
    /// Dependencies (e.g., tool_ids or external services)
    pub dependencies: Vec<String>,
}

/// Tool Registry entry stored in database
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRegistryEntry {
    pub tool_id: Uuid,
    pub name: String,
    pub version: String,
    pub spec: ToolSpec,
    pub status: String, // active, deprecated, disabled
    pub certified_by: String, // user_id or agent_id who certified
    pub certified_at: DateTime<Utc>,
    pub health_status: String, // healthy, degraded, unavailable
    pub last_health_check: Option<DateTime<Utc>>,
}

/// Health check result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckResult {
    pub tool_id: Uuid,
    pub status: String, // healthy, degraded, unavailable
    pub message: String,
    pub checked_at: DateTime<Utc>,
}

// ── Database Operations ────────────────────────────────────────────────────────

/// Register a new tool after Security certification
pub async fn register_tool(
    pool: &PgPool,
    name: String,
    version: String,
    spec: ToolSpec,
    certified_by: String,
) -> Result<ToolRegistryEntry> {
    let tool_id = Uuid::new_v4();
    let now = Utc::now();
    let spec_json = serde_json::to_value(&spec)?;

    sqlx::query(
        r#"
        INSERT INTO frank_tool_registry
        (tool_id, name, version, spec, status, certified_by, certified_at, health_status, last_health_check)
        VALUES ($1, $2, $3, $4, 'active', $5, $6, 'unknown', NULL)
        "#
    )
    .bind(&tool_id)
    .bind(&name)
    .bind(&version)
    .bind(&spec_json)
    .bind(&certified_by)
    .bind(&now)
    .execute(pool)
    .await?;

    info!("Tool registered: {} v{} ({})", name, version, tool_id);

    Ok(ToolRegistryEntry {
        tool_id,
        name,
        version,
        spec,
        status: "active".to_string(),
        certified_by,
        certified_at: now,
        health_status: "unknown".to_string(),
        last_health_check: None,
    })
}

/// List all tools
pub async fn list_tools(pool: &PgPool) -> Result<Vec<ToolRegistryEntry>> {
    let rows = sqlx::query(
        r#"
        SELECT tool_id, name, version, spec, status, certified_by, certified_at, health_status, last_health_check
        FROM frank_tool_registry
        ORDER BY certified_at DESC
        "#
    )
    .fetch_all(pool)
    .await?;

    let mut tools = Vec::new();
    for row in rows {
        let spec_json: Value = row.try_get("spec")?;
        let spec: ToolSpec = serde_json::from_value(spec_json)?;
        tools.push(ToolRegistryEntry {
            tool_id: row.try_get("tool_id")?,
            name: row.try_get("name")?,
            version: row.try_get("version")?,
            spec,
            status: row.try_get("status")?,
            certified_by: row.try_get("certified_by")?,
            certified_at: row.try_get("certified_at")?,
            health_status: row.try_get("health_status")?,
            last_health_check: row.try_get("last_health_check")?,
        });
    }

    Ok(tools)
}

/// Get tool details by ID
pub async fn get_tool(pool: &PgPool, tool_id: Uuid) -> Result<Option<ToolRegistryEntry>> {
    let row = sqlx::query(
        r#"
        SELECT tool_id, name, version, spec, status, certified_by, certified_at, health_status, last_health_check
        FROM frank_tool_registry
        WHERE tool_id = $1
        "#
    )
    .bind(&tool_id)
    .fetch_optional(pool)
    .await?;

    if let Some(row) = row {
        let spec_json: Value = row.try_get("spec")?;
        let spec: ToolSpec = serde_json::from_value(spec_json)?;
        return Ok(Some(ToolRegistryEntry {
            tool_id: row.try_get("tool_id")?,
            name: row.try_get("name")?,
            version: row.try_get("version")?,
            spec,
            status: row.try_get("status")?,
            certified_by: row.try_get("certified_by")?,
            certified_at: row.try_get("certified_at")?,
            health_status: row.try_get("health_status")?,
            last_health_check: row.try_get("last_health_check")?,
        }));
    }

    Ok(None)
}

/// Update tool health status
pub async fn update_health_status(
    pool: &PgPool,
    tool_id: Uuid,
    health_status: String,
    message: Option<String>,
) -> Result<HealthCheckResult> {
    let now = Utc::now();

    sqlx::query(
        r#"
        UPDATE frank_tool_registry
        SET health_status = $1, last_health_check = $2
        WHERE tool_id = $3
        "#
    )
    .bind(&health_status)
    .bind(&now)
    .bind(&tool_id)
    .execute(pool)
    .await?;

    // Log health event for auditing
    let _ = sqlx::query(
        r#"
        INSERT INTO frank_tool_health_log (tool_id, status, message, checked_at)
        VALUES ($1, $2, $3, $4)
        "#
    )
    .bind(&tool_id)
    .bind(&health_status)
    .bind(&message)
    .bind(&now)
    .execute(pool)
    .await;

    info!("Tool health updated: {} -> {}", tool_id, health_status);

    Ok(HealthCheckResult {
        tool_id,
        status: health_status,
        message: message.unwrap_or_default(),
        checked_at: now,
    })
}

/// Deprecate a tool (soft delete)
pub async fn deprecate_tool(pool: &PgPool, tool_id: Uuid) -> Result<()> {
    sqlx::query(
        r#"
        UPDATE frank_tool_registry
        SET status = 'deprecated'
        WHERE tool_id = $1
        "#
    )
    .bind(&tool_id)
    .execute(pool)
    .await?;

    info!("Tool deprecated: {}", tool_id);
    Ok(())
}

/// Get tools by status
pub async fn get_tools_by_status(pool: &PgPool, status: &str) -> Result<Vec<ToolRegistryEntry>> {
    let rows = sqlx::query(
        r#"
        SELECT tool_id, name, version, spec, status, certified_by, certified_at, health_status, last_health_check
        FROM frank_tool_registry
        WHERE status = $1
        ORDER BY certified_at DESC
        "#
    )
    .bind(status)
    .fetch_all(pool)
    .await?;

    let mut tools = Vec::new();
    for row in rows {
        let spec_json: Value = row.try_get("spec")?;
        let spec: ToolSpec = serde_json::from_value(spec_json)?;
        tools.push(ToolRegistryEntry {
            tool_id: row.try_get("tool_id")?,
            name: row.try_get("name")?,
            version: row.try_get("version")?,
            spec,
            status: row.try_get("status")?,
            certified_by: row.try_get("certified_by")?,
            certified_at: row.try_get("certified_at")?,
            health_status: row.try_get("health_status")?,
            last_health_check: row.try_get("last_health_check")?,
        });
    }

    Ok(tools)
}

/// Stub: Check if a tool is callable (full implementation in Gap 10)
pub async fn check_tool_health(tool_id: Uuid) -> Result<HealthCheckResult> {
    // For now, just mark as healthy
    // Gap 10 will implement actual health checks
    let now = Utc::now();

    Ok(HealthCheckResult {
        tool_id,
        status: "healthy".to_string(),
        message: "Health check passed (stub)".to_string(),
        checked_at: now,
    })
}
