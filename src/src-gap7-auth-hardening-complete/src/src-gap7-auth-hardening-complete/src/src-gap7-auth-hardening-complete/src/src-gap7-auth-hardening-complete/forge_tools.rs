//! Tool implementations for the Forge (process management) and Nexus (scheduling).
//! These functions are called from tools.rs execute_tool().

use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

use crate::forge::{Forge, ProcessStatus};
use crate::nexus::{self, TriggerPayload, TriggerSchedule};
use crate::swarm::{Swarm, SwarmTask, SwarmEvent};
use crate::delivery::DeliveryBus;
use crate::tools::ToolContext;
use crate::llm::StreamEvent;
use sqlx::PgPool;
use tokio::sync::mpsc;

// ── Forge tools ───────────────────────────────────────────────────────────────

pub async fn tool_process_spawn(input: &Value, forge: &Arc<Forge>) -> Value {
    let command = match input["command"].as_str() {
        Some(c) => c.to_string(),
        None => return json!({ "error": "command is required" }),
    };
    let cwd = input["cwd"].as_str();
    let timeout = input["timeout_secs"].as_u64();

    let env_vars: Option<Vec<(String, String)>> = input["env"].as_object().map(|obj| {
        obj.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect()
    });

    match forge.spawn(&command, cwd, env_vars, timeout).await {
        Ok(id) => json!({
            "ok": true,
            "process_id": id.to_string(),
            "message": format!("Process started. Use process_status('{}') to check on it.", id),
        }),
        Err(e) => json!({ "error": format!("Spawn failed: {}", e) }),
    }
}

pub async fn tool_process_status(input: &Value, forge: &Arc<Forge>) -> Value {
    let id_str = match input["process_id"].as_str() {
        Some(s) => s,
        None => return json!({ "error": "process_id required" }),
    };
    let id = match Uuid::parse_str(id_str) {
        Ok(id) => id,
        Err(_) => return json!({ "error": "invalid process_id" }),
    };

    match forge.get(id) {
        Some(p) => {
            let status = p.status_snapshot().await;
            json!({
                "process_id": id_str,
                "command": &p.command[..p.command.len().min(100)],
                "status": match &status {
                    ProcessStatus::Running => "running",
                    ProcessStatus::Exited { .. } => "exited",
                    ProcessStatus::Killed => "killed",
                    ProcessStatus::TimedOut => "timed_out",
                },
                "exit_code": match &status {
                    ProcessStatus::Exited { code } => Some(*code),
                    _ => None,
                },
                "elapsed_secs": p.elapsed_secs(),
                "cwd": p.cwd,
            })
        }
        None => json!({ "error": format!("Process {} not found", id_str) }),
    }
}

pub async fn tool_process_log(input: &Value, forge: &Arc<Forge>) -> Value {
    let id_str = match input["process_id"].as_str() {
        Some(s) => s,
        None => return json!({ "error": "process_id required" }),
    };
    let id = match Uuid::parse_str(id_str) {
        Ok(id) => id,
        Err(_) => return json!({ "error": "invalid process_id" }),
    };
    let lines = input["lines"].as_u64().unwrap_or(50) as usize;

    match forge.get(id) {
        Some(p) => {
            let output = p.tail_output(lines).await;
            let status = p.status_snapshot().await;
            json!({
                "process_id": id_str,
                "status": format!("{:?}", status),
                "lines": output,
                "elapsed_secs": p.elapsed_secs(),
            })
        }
        None => json!({ "error": format!("Process {} not found", id_str) }),
    }
}

pub async fn tool_process_write(input: &Value, forge: &Arc<Forge>) -> Value {
    let id_str = match input["process_id"].as_str() {
        Some(s) => s,
        None => return json!({ "error": "process_id required" }),
    };
    let data = match input["data"].as_str() {
        Some(d) => d.to_string(),
        None => return json!({ "error": "data required" }),
    };
    let id = match Uuid::parse_str(id_str) {
        Ok(id) => id,
        Err(_) => return json!({ "error": "invalid process_id" }),
    };

    // Append newline if not present (most CLIs expect it)
    let data = if data.ends_with('\n') { data } else { format!("{}\n", data) };

    match forge.get(id) {
        Some(p) => match p.write_stdin(&data).await {
            Ok(_) => json!({ "ok": true, "wrote": data.len() }),
            Err(e) => json!({ "error": format!("Write failed: {}", e) }),
        },
        None => json!({ "error": format!("Process {} not found", id_str) }),
    }
}

pub async fn tool_process_kill(input: &Value, forge: &Arc<Forge>) -> Value {
    let id_str = match input["process_id"].as_str() {
        Some(s) => s,
        None => return json!({ "error": "process_id required" }),
    };
    let id = match Uuid::parse_str(id_str) {
        Ok(id) => id,
        Err(_) => return json!({ "error": "invalid process_id" }),
    };

    match forge.kill(id).await {
        Ok(_) => json!({ "ok": true, "message": "Kill signal sent" }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

pub async fn tool_process_wait(input: &Value, forge: &Arc<Forge>) -> Value {
    let id_str = match input["process_id"].as_str() {
        Some(s) => s,
        None => return json!({ "error": "process_id required" }),
    };
    let timeout = input["timeout_secs"].as_u64().unwrap_or(300);
    let id = match Uuid::parse_str(id_str) {
        Ok(id) => id,
        Err(_) => return json!({ "error": "invalid process_id" }),
    };

    match forge.wait_for(id, timeout).await {
        Ok(status) => {
            // Grab the last 30 lines of output
            let output = if let Some(p) = forge.get(id) {
                p.tail_output(30).await
            } else {
                vec![]
            };
            json!({
                "process_id": id_str,
                "status": format!("{:?}", status),
                "exit_code": match &status {
                    ProcessStatus::Exited { code } => Some(*code),
                    _ => None,
                },
                "output_tail": output,
            })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

pub async fn tool_process_list(forge: &Arc<Forge>) -> Value {
    json!({ "processes": forge.list() })
}

// ── Nexus / Schedule tools ────────────────────────────────────────────────────

pub async fn tool_schedule_task(input: &Value, db: &sqlx::PgPool, user_id: Uuid) -> Value {
    let name = match input["name"].as_str() {
        Some(n) => n,
        None => return json!({ "error": "name required" }),
    };

    // Parse schedule
    let schedule_json = &input["schedule"];
    let schedule: TriggerSchedule = match serde_json::from_value(schedule_json.clone()) {
        Ok(s) => s,
        Err(e) => return json!({ "error": format!("Invalid schedule: {}. Use {{\"type\":\"once\",\"at\":\"ISO8601\"}} or {{\"type\":\"cron\",\"expr\":\"0 9 * * *\"}} or {{\"type\":\"interval_ms\",\"ms\":3600000}}", e) }),
    };

    // Parse payload
    let payload_json = &input["payload"];
    let payload: TriggerPayload = match serde_json::from_value(payload_json.clone()) {
        Ok(p) => p,
        Err(e) => return json!({ "error": format!("Invalid payload: {}. Use {{\"type\":\"agent_turn\",\"prompt\":\"...\"}} or {{\"type\":\"notify\",\"title\":\"...\",\"body\":\"...\"}} or {{\"type\":\"webhook\",\"url\":\"...\",\"body\":{{}}}}", e) }),
    };

    let max_fires: i32 = input["max_fires"].as_i64().unwrap_or(0) as i32;

    match nexus::create_trigger(db, name, &schedule, &payload, Some(user_id), max_fires).await {
        Ok(id) => json!({
            "ok": true,
            "trigger_id": id.to_string(),
            "name": name,
            "message": format!("Trigger '{}' created. Frank will fire it on schedule.", name),
        }),
        Err(e) => json!({ "error": format!("Failed to create trigger: {}", e) }),
    }
}

pub async fn tool_list_triggers(db: &sqlx::PgPool, user_id: Uuid) -> Value {
    match nexus::list_triggers(db, Some(user_id)).await {
        Ok(triggers) => json!({ "triggers": triggers, "count": triggers.len() }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

pub async fn tool_delete_trigger(input: &Value, db: &sqlx::PgPool) -> Value {
    let id_str = match input["trigger_id"].as_str() {
        Some(s) => s,
        None => return json!({ "error": "trigger_id required" }),
    };
    let id = match Uuid::parse_str(id_str) {
        Ok(id) => id,
        Err(_) => return json!({ "error": "invalid trigger_id" }),
    };

    match nexus::delete_trigger(db, id).await {
        Ok(_) => json!({ "ok": true }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── apply_patch ───────────────────────────────────────────────────────────────

/// Atomic multi-file patch application.
/// Parses the *** Begin Patch / *** End Patch format, validates all hunks,
/// then applies or rejects the whole patch atomically.
pub async fn tool_apply_patch(input: &Value) -> Value {
    let patch_text = match input["patch"].as_str() {
        Some(p) => p,
        None => return json!({ "error": "patch field required (*** Begin Patch ... *** End Patch)" }),
    };

    match apply_patch_atomic(patch_text) {
        Ok(report) => json!({ "ok": true, "files_modified": report.files_modified, "hunks_applied": report.hunks_applied }),
        Err(e) => json!({ "error": format!("Patch failed (no files changed): {}", e) }),
    }
}

#[derive(Debug)]
struct PatchReport {
    files_modified: Vec<String>,
    hunks_applied: usize,
}

fn apply_patch_atomic(patch: &str) -> anyhow::Result<PatchReport> {
    use similar::{ChangeTag, TextDiff};
    use std::collections::HashMap;

    // Strip Begin/End markers
    let body = patch
        .trim()
        .trim_start_matches("*** Begin Patch")
        .trim_end_matches("*** End Patch")
        .trim();

    // Parse file sections: *** path/to/file ***
    let mut file_sections: Vec<(String, String)> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_lines: Vec<&str> = Vec::new();

    for line in body.lines() {
        if line.starts_with("*** ") && line.ends_with(" ***") && !line.starts_with("*** Begin") && !line.starts_with("*** End") {
            if let Some(path) = current_path.take() {
                file_sections.push((path, current_lines.join("\n")));
                current_lines.clear();
            }
            let path = line.trim_start_matches("*** ").trim_end_matches(" ***").trim().to_string();
            current_path = Some(path);
        } else if current_path.is_some() {
            current_lines.push(line);
        }
    }
    if let Some(path) = current_path {
        file_sections.push((path, current_lines.join("\n")));
    }

    if file_sections.is_empty() {
        return Err(anyhow::anyhow!("No file sections found in patch"));
    }

    // Validate all files and apply hunks in memory first
    let mut staged: HashMap<String, String> = HashMap::new();
    let mut total_hunks = 0;

    for (path, diff_text) in &file_sections {
        let original = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Cannot read '{}': {}", path, e))?;

        let result = apply_unified_diff(&original, diff_text)?;
        total_hunks += result.1;
        staged.insert(path.clone(), result.0);
    }

    // All validated — now write to disk
    let mut modified = Vec::new();
    for (path, content) in staged {
        // Create parent dirs if needed
        if let Some(parent) = std::path::Path::new(&path).parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("Cannot create dirs for '{}': {}", path, e))?;
        }
        std::fs::write(&path, &content)
            .map_err(|e| anyhow::anyhow!("Cannot write '{}': {}", path, e))?;
        modified.push(path);
    }

    Ok(PatchReport { files_modified: modified, hunks_applied: total_hunks })
}

/// Apply unified diff hunks to original content. Returns (new_content, hunk_count).
fn apply_unified_diff(original: &str, diff: &str) -> anyhow::Result<(String, usize)> {
    let mut result_lines: Vec<String> = original.lines().map(String::from).collect();
    let mut hunk_count = 0;
    let mut offset: i64 = 0; // track line number shifts from previous hunks

    let hunk_re = regex_lite_hunk_header(diff);

    for hunk in parse_hunks(diff) {
        hunk_count += 1;
        let start_line = (hunk.orig_start as i64 - 1 + offset) as usize;
        let mut i = start_line;
        let mut new_lines: Vec<String> = Vec::new();
        let mut orig_consumed = 0usize;

        for op in &hunk.ops {
            match op {
                HunkOp::Context(line) => {
                    // Verify context matches
                    if i < result_lines.len() && result_lines[i].trim() != line.trim() {
                        return Err(anyhow::anyhow!(
                            "Context mismatch at line {}: expected '{}', got '{}'",
                            i + 1, line, result_lines[i]
                        ));
                    }
                    new_lines.push(result_lines.get(i).cloned().unwrap_or_default());
                    i += 1;
                    orig_consumed += 1;
                }
                HunkOp::Remove(line) => {
                    if i < result_lines.len() && result_lines[i].trim() != line.trim() {
                        return Err(anyhow::anyhow!(
                            "Remove mismatch at line {}: expected '{}', got '{}'",
                            i + 1, line, result_lines[i]
                        ));
                    }
                    i += 1;
                    orig_consumed += 1;
                    // Don't push — removed
                }
                HunkOp::Add(line) => {
                    new_lines.push(line.clone());
                    // offset increases by 1 for each added line
                }
            }
        }

        // Splice: replace [start_line .. start_line + orig_consumed] with new_lines
        let end = start_line + orig_consumed;
        let added = new_lines.len() as i64;
        let removed = orig_consumed as i64;
        offset += added - removed;

        result_lines.splice(start_line..end, new_lines);
    }

    Ok((result_lines.join("\n"), hunk_count))
}

#[derive(Debug)]
struct Hunk {
    orig_start: usize,
    ops: Vec<HunkOp>,
}

#[derive(Debug)]
enum HunkOp {
    Context(String),
    Remove(String),
    Add(String),
}

fn parse_hunks(diff: &str) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut current: Option<Hunk> = None;

    for line in diff.lines() {
        if line.starts_with("@@ ") {
            if let Some(h) = current.take() { hunks.push(h); }
            // Parse @@ -N,M +N,M @@ or @@ -N +N @@
            let orig_start = parse_hunk_header(line).unwrap_or(1);
            current = Some(Hunk { orig_start, ops: Vec::new() });
        } else if let Some(ref mut hunk) = current {
            if line.starts_with('-') {
                hunk.ops.push(HunkOp::Remove(line[1..].to_string()));
            } else if line.starts_with('+') {
                hunk.ops.push(HunkOp::Add(line[1..].to_string()));
            } else if line.starts_with(' ') {
                hunk.ops.push(HunkOp::Context(line[1..].to_string()));
            } else if line.starts_with('\\') {
                // "\ No newline at end of file" — ignore
            }
        }
    }
    if let Some(h) = current { hunks.push(h); }
    hunks
}

fn parse_hunk_header(line: &str) -> Option<usize> {
    // @@ -N[,M] +N[,M] @@
    let after_at = line.strip_prefix("@@ -")?;
    let num_end = after_at.find(|c: char| !c.is_ascii_digit())?;
    after_at[..num_end].parse().ok()
}

fn regex_lite_hunk_header(_diff: &str) {} // placeholder, not actually needed

/// Execute a sequence of tools without LLM round-trips.
/// Input: { "steps": [{"tool": "tool_name", "input": {...}}, ...] }
/// Returns: { "results": [...], "all_success": bool }
pub async fn exec_tool_pipeline(input: &Value, ctx: &crate::tools::ToolContext) -> Value {
    use crate::tools::execute_tool;
    
    let steps = match input["steps"].as_array() {
        Some(arr) => arr,
        None => return json!({ "error": "steps array required" }),
    };

    if steps.is_empty() {
        return json!({ "error": "steps array must not be empty" });
    }

    if steps.len() > 20 {
        return json!({ "error": "maximum 20 steps allowed per pipeline" });
    }

    let mut results = Vec::new();
    let mut all_success = true;

    for (idx, step) in steps.iter().enumerate() {
        let tool_name = match step["tool"].as_str() {
            Some(name) => name,
            None => {
                results.push(json!({
                    "step": idx,
                    "error": "missing 'tool' field in step"
                }));
                all_success = false;
                break;
            }
        };

        // Prevent recursive tool_pipeline calls
        if tool_name == "tool_pipeline" {
            results.push(json!({
                "step": idx,
                "tool": tool_name,
                "error": "tool_pipeline cannot call itself (no nested pipelines)"
            }));
            all_success = false;
            break;
        }

        let tool_input = &step["input"];
        if !tool_input.is_object() {
            results.push(json!({
                "step": idx,
                "tool": tool_name,
                "error": "missing or invalid 'input' field in step (must be object)"
            }));
            all_success = false;
            break;
        }

        // Execute tool directly via execute_tool
        let result = execute_tool(tool_name, tool_input, ctx).await;

        let step_success = result.success;
        all_success = all_success && step_success;

        results.push(json!({
            "step": idx,
            "tool": tool_name,
            "success": step_success,
            "output": result.output,
            "duration_ms": result.duration_ms
        }));

        // Stop pipeline on first failure
        if !step_success {
            break;
        }
    }

    json!({
        "ok": true,
        "results": results,
        "all_success": all_success,
        "steps_executed": results.len()
    })
}
