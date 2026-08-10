//! Skills system tools — Gap 4
//! save, load, list, and use reusable procedures

use anyhow::{Context, Result};
use serde_json::{json, Value};
use sqlx::PgPool;
use sqlx::Row;

pub async fn skill_save(pool: &PgPool, params: &Value) -> Result<Value> {
    let name        = params["name"].as_str().context("skill_save: name required")?;
    let description = params["description"].as_str().context("skill_save: description required")?;
    let steps       = params.get("steps").cloned().unwrap_or(json!([]));
    let tags: Vec<String> = params["tags"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let row = sqlx::query(
        "INSERT INTO frank_skills (name, description, steps, tags, updated_at)
         VALUES ($1, $2, $3, $4, NOW())
         ON CONFLICT (name) DO UPDATE
         SET description = EXCLUDED.description,
             steps = EXCLUDED.steps,
             tags = EXCLUDED.tags,
             updated_at = NOW()
         RETURNING id, name"
    )
    .bind(name)
    .bind(description)
    .bind(steps)
    .bind(&tags)
    .fetch_one(pool).await?;

    let id: uuid::Uuid = row.try_get("id")?;
    let name_out: String = row.try_get("name")?;
    Ok(json!({ "success": true, "skill_id": id.to_string(), "name": name_out, "message": "Skill saved" }))
}

pub async fn skill_load(pool: &PgPool, params: &Value) -> Result<Value> {
    let name = params["name"].as_str();
    let id_str = params["id"].as_str();

    if name.is_none() && id_str.is_none() {
        anyhow::bail!("skill_load: name or id required");
    }

    let row = if let Some(n) = name {
        sqlx::query(
            "SELECT id, name, description, steps, tags, use_count, last_used_at, created_at
             FROM frank_skills WHERE name = $1"
        ).bind(n).fetch_optional(pool).await?
    } else {
        let id: uuid::Uuid = id_str.unwrap().parse().context("invalid uuid")?;
        sqlx::query(
            "SELECT id, name, description, steps, tags, use_count, last_used_at, created_at
             FROM frank_skills WHERE id = $1"
        ).bind(id).fetch_optional(pool).await?
    };

    match row {
        None => Ok(json!({ "error": "Skill not found" })),
        Some(r) => {
            let id: uuid::Uuid     = r.try_get("id")?;
            let name: String       = r.try_get("name")?;
            let desc: String       = r.try_get("description")?;
            let steps: Value       = r.try_get("steps")?;
            let tags: Vec<String>  = r.try_get("tags")?;
            let use_count: i32     = r.try_get("use_count")?;
            Ok(json!({ "id": id.to_string(), "name": name, "description": desc,
                        "steps": steps, "tags": tags, "use_count": use_count }))
        }
    }
}

pub async fn skill_list(pool: &PgPool, params: &Value) -> Result<Value> {
    let tag    = params["tag"].as_str();
    let search = params["search"].as_str();
    let limit  = params["limit"].as_i64().unwrap_or(50).min(200) as i32;

    let rows = if let Some(t) = tag {
        sqlx::query(
            "SELECT id, name, description, tags, use_count FROM frank_skills
             WHERE $1 = ANY(tags) ORDER BY use_count DESC, name ASC LIMIT $2"
        ).bind(t).bind(limit).fetch_all(pool).await?
    } else if let Some(s) = search {
        let pattern = format!("%{}%", s);
        sqlx::query(
            "SELECT id, name, description, tags, use_count FROM frank_skills
             WHERE name ILIKE $1 OR description ILIKE $1
             ORDER BY use_count DESC, name ASC LIMIT $2"
        ).bind(&pattern).bind(limit).fetch_all(pool).await?
    } else {
        sqlx::query(
            "SELECT id, name, description, tags, use_count FROM frank_skills
             ORDER BY use_count DESC, name ASC LIMIT $1"
        ).bind(limit).fetch_all(pool).await?
    };

    let skills: Vec<Value> = rows.iter().map(|r| {
        let id: uuid::Uuid    = r.try_get("id").unwrap_or_default();
        let name: String      = r.try_get("name").unwrap_or_default();
        let desc: String      = r.try_get("description").unwrap_or_default();
        let tags: Vec<String> = r.try_get("tags").unwrap_or_default();
        let use_count: i32    = r.try_get("use_count").unwrap_or(0);
        json!({ "id": id.to_string(), "name": name, "description": desc,
                 "tags": tags, "use_count": use_count })
    }).collect();

    Ok(json!({ "skills": skills, "count": skills.len() }))
}

pub async fn skill_use(pool: &PgPool, params: &Value) -> Result<Value> {
    let name = params["name"].as_str().context("skill_use: name required")?;

    let row = sqlx::query(
        "UPDATE frank_skills SET use_count = use_count + 1, last_used_at = NOW()
         WHERE name = $1
         RETURNING id, name, description, steps, tags, use_count"
    ).bind(name).fetch_optional(pool).await?;

    match row {
        None => Ok(json!({ "error": "Skill not found" })),
        Some(r) => {
            let id: uuid::Uuid    = r.try_get("id")?;
            let name: String      = r.try_get("name")?;
            let desc: String      = r.try_get("description")?;
            let steps: Value      = r.try_get("steps")?;
            let tags: Vec<String> = r.try_get("tags")?;
            let use_count: i32    = r.try_get("use_count")?;
            Ok(json!({ "id": id.to_string(), "name": name, "description": desc,
                        "steps": steps, "tags": tags, "use_count": use_count }))
        }
    }
}
