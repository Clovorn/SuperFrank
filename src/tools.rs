//! FrankOS Tool Runtime — all tools Frank can execute without calling an LLM

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::process::Command;
use tracing::{info, warn};
use futures::FutureExt; // Gap 8B.5: For catch_unwind on tool panics

// ── Tool Definitions (sent to LLM as tool specs) ──────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// All tools Frank can call — returned to LLM in every request
pub fn all_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "shell_exec".into(),
            description: "Execute a shell command on the FrankOS server. Use for builds, deployments, process management, and any server operation. Returns stdout, stderr, and exit code.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The shell command to run" },
                    "workdir": { "type": "string", "description": "Working directory (optional, defaults to /opt/frankos)" },
                    "timeout_secs": { "type": "integer", "description": "Timeout in seconds (optional, default 60)" }
                },
                "required": ["command"]
            }),
        },
        ToolDef {
            name: "file_read".into(),
            description: "Read the contents of a file on the server. Supports text files. Returns file content as a string.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute or relative path to the file" },
                    "offset": { "type": "integer", "description": "Line number to start reading from (1-indexed, optional)" },
                    "limit": { "type": "integer", "description": "Max lines to read (optional)" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "file_write".into(),
            description: "Write content to a file on the server. Creates the file and any parent directories if they don't exist. Overwrites existing files.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute or relative path to write" },
                    "content": { "type": "string", "description": "Content to write" }
                },
                "required": ["path", "content"]
            }),
        },
        ToolDef {
            name: "file_edit".into(),
            description: "Edit a file by replacing exact text. The old_text must match exactly and uniquely in the file.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to file" },
                    "old_text": { "type": "string", "description": "Exact text to find and replace" },
                    "new_text": { "type": "string", "description": "Replacement text" }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        },
        ToolDef {
            name: "file_list".into(),
            description: "List files and directories at a given path.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory to list" },
                    "recursive": { "type": "boolean", "description": "List recursively (default false)" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "git_status".into(),
            description: "Get git status, diff, log, or other git info for a repository on the server.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_path": { "type": "string", "description": "Path to git repository" },
                    "command": { "type": "string", "description": "Git subcommand: status|diff|log|branch|show (default: status)" }
                },
                "required": ["repo_path"]
            }),
        },
        ToolDef {
            name: "git_commit".into(),
            description: "Stage all changes and commit in a git repository.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_path": { "type": "string", "description": "Path to git repository" },
                    "message": { "type": "string", "description": "Commit message" },
                    "push": { "type": "boolean", "description": "Also push after commit (default false)" }
                },
                "required": ["repo_path", "message"]
            }),
        },
        ToolDef {
            name: "web_search".into(),
            description: "Search the web using Brave Search. Returns titles, URLs, and snippets.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "count": { "type": "integer", "description": "Number of results (default 5, max 10)" }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "web_fetch".into(),
            description: "Fetch and extract readable text content from a URL.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "URL to fetch" },
                    "max_chars": { "type": "integer", "description": "Max characters to return (default 8000)" }
                },
                "required": ["url"]
            }),
        },
        ToolDef {
            name: "memory_write".into(),
            description: "Store a fact, insight, decision, or preference to Frank's long-term memory. Use this to remember things across conversations.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short title for this memory" },
                    "content": { "type": "string", "description": "What to remember" },
                    "memory_type": { "type": "string", "description": "Type: decision|concept|preference|lesson|telos|project" },
                    "importance": { "type": "integer", "description": "Importance 1-10 (default 5)" },
                    "namespace": { "type": "string", "description": "Memory namespace (default: chuck_frank)" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Tags for retrieval" }
                },
                "required": ["title", "content"]
            }),
        },
        ToolDef {
            name: "memory_search".into(),
            description: "Search Frank's memory for relevant stored information.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to search for" },
                    "limit": { "type": "integer", "description": "Max results (default 5)" },
                    "namespace": { "type": "string", "description": "Memory namespace (default: chuck_frank)" }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "memory_search_semantic".into(),
            description: "Semantic vector search across Frank's memory using embeddings. Better for conceptual queries than keyword search. Returns memories ranked by semantic similarity.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to search for (will be embedded)" },
                    "limit": { "type": "integer", "description": "Max results (default 5, max 20)" },
                    "namespace": { "type": "string", "description": "Memory namespace (default: chuck_frank)" },
                    "threshold": { "type": "number", "description": "Similarity threshold 0-1 (default 0.3)" }
                },
                "required": ["query"]
            }),
        },
        ToolDef {
            name: "memory_list".into(),
            description: "List Frank's stored memories. Filter by bucket, type, or tag to review and organize.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "bucket": { "type": "string", "description": "personal|work|training|identity|all (default: all)" },
                    "memory_type": { "type": "string", "description": "Filter by type (optional)" },
                    "tag": { "type": "string", "description": "Filter by tag (optional)" },
                    "limit": { "type": "integer", "description": "Max results (default 20)" }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "memory_update".into(),
            description: "Update an existing memory entry — improve its content, adjust importance, or add tags.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Title of the memory to update (fuzzy match)" },
                    "content": { "type": "string", "description": "New content (omit to keep existing)" },
                    "importance": { "type": "integer", "description": "New importance 1-10 (omit to keep existing)" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "New tags (omit to keep existing)" }
                },
                "required": ["title"]
            }),
        },
        ToolDef {
            name: "memory_delete".into(),
            description: "Delete a memory entry that is wrong, outdated, or no longer relevant.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Title of the memory to delete (fuzzy match)" },
                    "bucket": { "type": "string", "description": "Bucket to search in (optional, searches all if omitted)" }
                },
                "required": ["title"]
            }),
        },
        ToolDef {
            name: "memory_move".into(),
            description: "Move a memory entry to a different bucket to reorganize thoughts across personal/work/training/identity.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Title of the memory to move (fuzzy match)" },
                    "to_bucket": { "type": "string", "description": "Destination: personal|work|training|identity" }
                },
                "required": ["title", "to_bucket"]
            }),
        },
        ToolDef {
            name: "spawn_agent".into(),
            description: "Spawn a worker agent to complete a focused task in parallel. The agent runs independently and returns results. Use for parallelizable work like research + coding simultaneously.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Agent name/role (e.g. 'researcher', 'coder', 'tester')" },
                    "goal": { "type": "string", "description": "Clear description of what this agent should accomplish" },
                    "tools": { "type": "array", "items": { "type": "string" }, "description": "Tools this agent can use. Leave empty to allow all tools." },
                    "model": { "type": "string", "description": "Model to use: haiku (default, fast/cheap), sonnet (complex), opus (deep reasoning)" }
                },
                "required": ["name", "goal"]
            }),
        },
        ToolDef {
            name: "service_ctl".into(),
            description: "Manage systemd services on the FrankOS server: start, stop, restart, status, enable, disable.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "service": { "type": "string", "description": "Service name (e.g. frankos-gateway)" },
                    "action": { "type": "string", "description": "Action: status|start|stop|restart|enable|disable" }
                },
                "required": ["service", "action"]
            }),
        },
        ToolDef {
            name: "cargo_build".into(),
            description: "Build a Rust project with cargo. Returns build output and any errors.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to Cargo.toml directory" },
                    "release": { "type": "boolean", "description": "Build in release mode (default true)" },
                    "bin": { "type": "string", "description": "Specific binary to build (optional)" }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "process_list".into(),
            description: "List running processes on the FrankOS server, optionally filtered by name.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "filter": { "type": "string", "description": "Filter processes by name (optional)" }
                }
            }),
        },
        // ── Expanded Capability Pack ──────────────────────────────────────────
        ToolDef { name: "send_email".into(), description: "Send an email via Resend to any address.".into(), input_schema: json!({"type":"object","properties":{"to":{"type":"string"},"subject":{"type":"string"},"body":{"type":"string"},"from":{"type":"string"}},"required":["to","subject","body"]}) },
        ToolDef { name: "github_list_repos".into(), description: "List GitHub repos for a user or org.".into(), input_schema: json!({"type":"object","properties":{"owner":{"type":"string"},"kind":{"type":"string"}},"required":["owner"]}) },
        ToolDef { name: "github_get_repo".into(), description: "Get details for a GitHub repo.".into(), input_schema: json!({"type":"object","properties":{"owner":{"type":"string"},"repo":{"type":"string"}},"required":["owner","repo"]}) },
        ToolDef { name: "github_list_issues".into(), description: "List issues for a GitHub repo.".into(), input_schema: json!({"type":"object","properties":{"owner":{"type":"string"},"repo":{"type":"string"},"state":{"type":"string"},"labels":{"type":"string"}},"required":["owner","repo"]}) },
        ToolDef { name: "github_create_issue".into(), description: "Create a GitHub issue.".into(), input_schema: json!({"type":"object","properties":{"owner":{"type":"string"},"repo":{"type":"string"},"title":{"type":"string"},"body":{"type":"string"},"labels":{"type":"array","items":{"type":"string"}}},"required":["owner","repo","title"]}) },
        ToolDef { name: "github_list_prs".into(), description: "List pull requests for a GitHub repo.".into(), input_schema: json!({"type":"object","properties":{"owner":{"type":"string"},"repo":{"type":"string"},"state":{"type":"string"}},"required":["owner","repo"]}) },
        ToolDef { name: "github_create_pr".into(), description: "Create a pull request on GitHub.".into(), input_schema: json!({"type":"object","properties":{"owner":{"type":"string"},"repo":{"type":"string"},"title":{"type":"string"},"head":{"type":"string"},"base":{"type":"string"},"body":{"type":"string"}},"required":["owner","repo","title","head"]}) },
        ToolDef { name: "github_get_file".into(), description: "Get file contents from a GitHub repo.".into(), input_schema: json!({"type":"object","properties":{"owner":{"type":"string"},"repo":{"type":"string"},"path":{"type":"string"},"branch":{"type":"string"}},"required":["owner","repo","path"]}) },
        ToolDef { name: "github_search_code".into(), description: "Search code across GitHub.".into(), input_schema: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}) },
        ToolDef { name: "cf_list_dns".into(), description: "List Cloudflare DNS records.".into(), input_schema: json!({"type":"object","properties":{"zone_id":{"type":"string"},"type":{"type":"string"}}}) },
        ToolDef { name: "cf_create_dns".into(), description: "Create a Cloudflare DNS record.".into(), input_schema: json!({"type":"object","properties":{"zone_id":{"type":"string"},"type":{"type":"string"},"name":{"type":"string"},"content":{"type":"string"},"ttl":{"type":"integer"},"proxied":{"type":"boolean"}},"required":["type","name","content"]}) },
        ToolDef { name: "cf_delete_dns".into(), description: "Delete a Cloudflare DNS record.".into(), input_schema: json!({"type":"object","properties":{"zone_id":{"type":"string"},"record_id":{"type":"string"}},"required":["record_id"]}) },
        ToolDef { name: "cf_purge_cache".into(), description: "Purge Cloudflare cache for a zone.".into(), input_schema: json!({"type":"object","properties":{"zone_id":{"type":"string"},"urls":{"type":"array","items":{"type":"string"}}}}) },
        ToolDef { name: "hetzner_list_servers".into(), description: "List all Hetzner Cloud servers.".into(), input_schema: json!({"type":"object","properties":{}}) },
        ToolDef { name: "hetzner_get_server".into(), description: "Get details for a Hetzner server by ID.".into(), input_schema: json!({"type":"object","properties":{"server_id":{"type":"integer"}},"required":["server_id"]}) },
        ToolDef { name: "hetzner_server_action".into(), description: "Perform an action on a Hetzner server (reboot/reset/poweron/poweroff/shutdown).".into(), input_schema: json!({"type":"object","properties":{"server_id":{"type":"integer"},"action":{"type":"string"}},"required":["server_id","action"]}) },
        ToolDef { name: "get_weather".into(), description: "Get current weather for a location. Returns structured JSON.".into(), input_schema: json!({"type":"object","properties":{"location":{"type":"string"},"format":{"type":"string"}},"required":["location"]}) },
        ToolDef { name: "notion_search".into(), description: "Search Notion workspace (requires NOTION_TOKEN).".into(), input_schema: json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}) },
        ToolDef { name: "notion_get_page".into(), description: "Get a Notion page by ID.".into(), input_schema: json!({"type":"object","properties":{"page_id":{"type":"string"}},"required":["page_id"]}) },
        ToolDef { name: "notion_create_page".into(), description: "Create a Notion page under a parent.".into(), input_schema: json!({"type":"object","properties":{"parent_id":{"type":"string"},"title":{"type":"string"},"content":{"type":"string"}},"required":["parent_id","title"]}) },
        ToolDef { name: "notion_append_block".into(), description: "Append content to a Notion page block.".into(), input_schema: json!({"type":"object","properties":{"block_id":{"type":"string"},"content":{"type":"string"}},"required":["block_id","content"]}) },
        ToolDef { name: "generate_image_openai".into(), description: "Generate an image with DALL-E 3 (OpenAI). Returns a URL.".into(), input_schema: json!({"type":"object","properties":{"prompt":{"type":"string"},"size":{"type":"string"},"quality":{"type":"string"}},"required":["prompt"]}) },
        ToolDef { name: "make_diagram".into(), description: "Generate an SVG diagram from a description using GPT-4o.".into(), input_schema: json!({"type":"object","properties":{"description":{"type":"string"},"filename":{"type":"string"}},"required":["description"]}) },
        ToolDef { name: "summarize".into(), description: "Summarize text or a URL. Styles: concise, detailed, bullet, one-sentence.".into(), input_schema: json!({"type":"object","properties":{"text":{"type":"string"},"url":{"type":"string"},"style":{"type":"string"}}}) },

    ]
}

/// Convert ToolDef vec to Anthropic tool spec format
pub fn to_anthropic_tools(tools: &[ToolDef]) -> Vec<Value> {
    tools.iter().map(|t| json!({
        "name": t.name,
        "description": t.description,
        "input_schema": t.input_schema,
    })).collect()
}

// ── Tool Executor ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub brave_api_key: Option<String>,
    pub google_ai_key: Option<String>,
    pub google_ai_project: Option<String>,
    pub luma_api_key: Option<String>,
    pub openai_api_key: Option<String>,
    pub db: sqlx::PgPool,
    pub session_id: uuid::Uuid,
    pub user_id: uuid::Uuid,
    pub chat_bucket: String,
    pub chat_folder: Option<String>,
    // v3 additions
    pub forge: Option<std::sync::Arc<crate::forge::Forge>>,
}

#[derive(Debug, Serialize)]
pub struct ToolResult {
    pub tool_name: String,
    pub success: bool,
    pub output: Value,
    pub duration_ms: u64,
}

pub fn execute_tool<'a>(

    tool_name: &'a str,
    input: &'a Value,
    ctx: &'a ToolContext,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ToolResult> + Send + 'a>> {
    Box::pin(async move {
        let start = std::time::Instant::now();
        info!("Executing tool: {} with input: {}", tool_name, input);
    
        // Gap 8B.5: Wrap entire tool dispatch in panic handler
        let result = std::panic::AssertUnwindSafe(async {
            match tool_name {
            "shell_exec"   => exec_shell(input).await,
            "file_read"    => exec_file_read(input).await,
            "file_write"   => {
                let result = exec_file_write(input).await;
                // Gap 7D: Auto-memory after architecture file writes
                if let Ok(ref output) = result {
                    if let Some(path) = input["path"].as_str() {
                        let goal_id = get_active_goal_id(&ctx.db, ctx.user_id).await;
                        let db = ctx.db.clone();
                        let path = path.to_string();
                        tokio::spawn(async move {
                            crate::auto_memory::after_architecture_write(&db, &path, goal_id).await;
                        });
                    }
                }
                result
            }
            "file_edit"    => exec_file_edit(input).await,
            "file_list"    => exec_file_list(input).await,
            "git_status"   => exec_git(input, "status").await,
            "git_commit"   => exec_git_commit(input).await,
            "web_search"   => exec_web_search(input, ctx.brave_api_key.as_deref()).await,
            "web_fetch"    => exec_web_fetch(input).await,
            "memory_write"  => exec_memory_write(input, ctx).await,
            "memory_search" => exec_memory_search(input, ctx).await,
            "memory_search_semantic" => exec_memory_search_semantic(input, ctx).await,
            "memory_list"   => exec_memory_list(input, ctx).await,
            "memory_update" => exec_memory_update(input, ctx).await,
            "memory_delete" => exec_memory_delete(input, ctx).await,
            "memory_move"   => exec_memory_move(input, ctx).await,
            "spawn_agent"  => exec_spawn_agent(input, ctx).await,
            "service_ctl"  => {
                let result = exec_service_ctl(input).await;
                // Gap 7D: Auto-memory after service restart
                if let Ok(ref output) = result {
                    if let (Some(service), Some(action)) = (input["service"].as_str(), input["action"].as_str()) {
                        if action == "restart" && output["exit_code"].as_i64() == Some(0) {
                            let goal_id = get_active_goal_id(&ctx.db, ctx.user_id).await;
                            let db = ctx.db.clone();
                            let service = service.to_string();
                            tokio::spawn(async move {
                                crate::auto_memory::after_service_restart(&db, &service, true, goal_id).await;
                            });
                        }
                    }
                }
                result
            }
            "cargo_build"  => {
                let result = exec_cargo_build(input).await;
                // Gap 7D: Auto-memory after successful build
                if let Ok(ref output) = result {
                    if output["success"].as_bool() == Some(true) {
                        if let Some(path) = input["path"].as_str() {
                            let goal_id = get_active_goal_id(&ctx.db, ctx.user_id).await;
                            let db = ctx.db.clone();
                            let path = path.to_string();
                            tokio::spawn(async move {
                                crate::auto_memory::after_cargo_build(&db, &path, true, goal_id).await;
                            });
                        }
                    }
                }
                result
            }
            "process_list"    => exec_process_list(input).await,
            "generate_image"  => exec_generate_image(input, ctx).await,
            "generate_video"  => exec_generate_video(input, ctx).await,
            "analyze_image"   => exec_analyze_image(input, ctx).await,
            "gemini_research" => exec_gemini_research(input, ctx).await,
            "gemini_chat"          => exec_gemini_chat(input, ctx).await,
            "luma_text_to_video"   => exec_luma_text_to_video(input, ctx).await,
            "luma_image_to_video"  => exec_luma_image_to_video(input, ctx).await,
            "luma_text_to_image"   => exec_luma_text_to_image(input, ctx).await,
            "luma_image_reference" => exec_luma_image_reference(input, ctx).await,
            "luma_style_reference" => exec_luma_style_reference(input, ctx).await,
            "luma_list_concepts"   => exec_luma_list_concepts(ctx).await,
            "luma_list_generations"=> exec_luma_list_generations(input, ctx).await,
            
            // Gap 10A: Compound Internal Tools
            "build_and_deploy"  => exec_build_and_deploy(input).await,
            "db_migration"      => exec_db_migration(input, ctx).await,
            "agent_spawn"       => exec_agent_spawn_fixed(input, ctx).await,
            "memory_commit"     => exec_memory_commit(input, ctx).await,
            
            // Gap 10B: Escalation Mailbox Tools
            "mailbox_write"     => exec_mailbox_write(input, ctx).await,
            "mailbox_read"      => exec_mailbox_read(input, ctx).await,
            "mailbox_mark_read" => exec_mailbox_mark_read(input, ctx).await,
    
            // Task management tools — Engineer task queue
            "task_list_pending" => Ok(crate::task_tools::exec_task_list_pending(&ctx.db).await
                .unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}))),
            "task_claim" => Ok(crate::task_tools::exec_task_claim(input, &ctx.db).await
                .unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}))),
            "task_done" => Ok(crate::task_tools::exec_task_done(input, &ctx.db).await
                .unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}))),
            "task_block" => Ok(crate::task_tools::exec_task_block(input, &ctx.db).await
                .unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}))),
            "notify_internal" => exec_notify_internal(input, ctx).await,
            "notification_inbox" => exec_notification_inbox(input, ctx).await,
            "notification_ack" => exec_notification_ack(input, ctx).await,
    
            // ── Expanded Capability Pack ──────────────────────────────────────────
            "send_email"            => exec_send_email(input, ctx).await,
            "github_list_repos"     => exec_github_list_repos(input, ctx).await,
            "github_get_repo"       => exec_github_get_repo(input, ctx).await,
            "github_list_issues"    => exec_github_list_issues(input, ctx).await,
            "github_create_issue"   => exec_github_create_issue(input, ctx).await,
            "github_list_prs"       => exec_github_list_prs(input, ctx).await,
            "github_create_pr"      => exec_github_create_pr(input, ctx).await,
            "github_get_file"       => exec_github_get_file(input, ctx).await,
            "github_search_code"    => exec_github_search_code(input, ctx).await,
            "cf_list_dns"           => exec_cf_list_dns(input, ctx).await,
            "cf_create_dns"         => exec_cf_create_dns(input, ctx).await,
            "cf_delete_dns"         => exec_cf_delete_dns(input, ctx).await,
            "cf_purge_cache"        => exec_cf_purge_cache(input, ctx).await,
            "hetzner_list_servers"  => exec_hetzner_list_servers(input, ctx).await,
            "hetzner_get_server"    => exec_hetzner_get_server(input, ctx).await,
            "hetzner_server_action" => exec_hetzner_server_action(input, ctx).await,
            "get_weather"           => exec_get_weather(input, ctx).await,
            "notion_search"         => exec_notion_search(input, ctx).await,
            "notion_get_page"       => exec_notion_get_page(input, ctx).await,
            "notion_create_page"    => exec_notion_create_page(input, ctx).await,
            "notion_append_block"   => exec_notion_append_block(input, ctx).await,
            "generate_image_openai" => exec_generate_image_openai(input, ctx).await,
            "make_diagram"          => exec_make_diagram(input, ctx).await,
            "summarize"             => exec_summarize(input, ctx).await,
    
            other => {
                // Try v3 tools (Forge, Nexus, apply_patch)
                match execute_v3_tool(other, input, ctx).await {
                    Some(output) => Ok(output),
                    None => Err(anyhow!("Unknown tool: {}", other)),
                }
            }
        }
        })
        .catch_unwind()
        .await;
    
        let duration_ms = start.elapsed().as_millis() as u64;
    
        // Gap 8B.5: Handle panic vs normal error
        let final_result = match result {
            Ok(tool_result) => tool_result, // Normal execution (Ok or Err from tool)
            Err(panic_info) => {
                // Tool panicked — log it and return error to agent
                tracing::error!("Tool '{}' PANICKED: {:?}", tool_name, panic_info);
                // Gap 8A: emit nexus_panic event
                let panic_str = format!("{:?}", panic_info);
                crate::events::emit_tool_failure(
                    &ctx.db,
                    tool_name,
                    &format!("PANIC: {}", { let ci = panic_str.char_indices().nth(200).map(|(i,_)|i).unwrap_or(panic_str.len()); &panic_str[..ci] }),
                    None,
                ).await;
                Err(anyhow!("Tool panicked during execution. Check logs for details."))
            }
        };
    
        match final_result {
            Ok(output) => ToolResult { tool_name: tool_name.to_string(), success: true, output, duration_ms },
            Err(e) => {
                warn!("Tool {} failed: {}", tool_name, e);
                // Gap 8A: emit tool_failure event
                let input_preview = serde_json::to_string(input).ok();
                let preview = input_preview.as_deref().map(|s| { let ci = s.char_indices().nth(200).map(|(i,_)|i).unwrap_or(s.len()); &s[..ci] });
                crate::events::emit_tool_failure(
                    &ctx.db,
                    tool_name,
                    &e.to_string(),
                    preview,
                ).await;
                ToolResult {
                    tool_name: tool_name.to_string(),
                    success: false,
                    output: json!({ "error": e.to_string() }),
                    duration_ms,
                }
            }
        }
    })
}

// ── Shell Exec ────────────────────────────────────────────────────────────────

async fn exec_shell(input: &Value) -> Result<Value> {
    let command = input["command"].as_str().ok_or_else(|| anyhow!("command required"))?;
    let workdir = input["workdir"].as_str().unwrap_or("/opt/frankos");
    let timeout_secs = input["timeout_secs"].as_u64().unwrap_or(60);

    // CRITICAL SAFETY: block any shell command that would stop or kill frankos-gateway.
    // SuperFrank cannot shut itself down — doing so kills the entire system with no recovery path.
    // Deployments must be performed externally by Mac Frank.
    let cmd_lower = command.to_lowercase();
    let is_suicide = (cmd_lower.contains("systemctl") && cmd_lower.contains("frankos-gateway")
        && (cmd_lower.contains(" stop") || cmd_lower.contains(" restart") || cmd_lower.contains(" kill")))
        || (cmd_lower.contains("kill") && cmd_lower.contains("frankos-gateway"))
        || cmd_lower.contains("kill 216903")  // guard against pid-specific kills
        || (cmd_lower.contains("kill ") && cmd_lower.contains("$(pgrep frankos"));
    if is_suicide {
        return Ok(json!({
            "success": false,
            "error": "SAFETY BLOCK: shell commands that stop or kill frankos-gateway are not permitted from within the service. Write READY_FOR_DEPLOYMENT to FRANK_TO_MAC.md — Mac Frank handles all frankos-gateway deployments externally.",
            "command": command
        }));
    }

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(workdir)
            .env("PATH", "/root/.cargo/bin:/root/.nvm/versions/node/v22.23.2/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
            .env("HOME", "/root")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
    ).await
    .map_err(|_| anyhow!("Command timed out after {}s", timeout_secs))??;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    Ok(json!({
        "stdout": stdout,
        "stderr": stderr,
        "exit_code": exit_code,
        "success": exit_code == 0
    }))
}

// ── File Operations ───────────────────────────────────────────────────────────

async fn exec_file_read(input: &Value) -> Result<Value> {
    let path = input["path"].as_str().ok_or_else(|| anyhow!("path required"))?;
    let content = tokio::fs::read_to_string(path).await
        .map_err(|e| anyhow!("Failed to read {}: {}", path, e))?;

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    let offset = input["offset"].as_u64().unwrap_or(1).saturating_sub(1) as usize;
    let limit = input["limit"].as_u64().unwrap_or(2000) as usize;

    let selected: Vec<&str> = lines.iter().skip(offset).take(limit).cloned().collect();
    let result = selected.join("\n");

    Ok(json!({
        "content": result,
        "total_lines": total_lines,
        "returned_lines": selected.len(),
        "offset": offset + 1,
    }))
}

async fn exec_file_write(input: &Value) -> Result<Value> {
    let path = input["path"].as_str().ok_or_else(|| anyhow!("path required"))?;
    let content = input["content"].as_str().ok_or_else(|| anyhow!("content required"))?;

    // Create parent directories
    if let Some(parent) = std::path::Path::new(path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    tokio::fs::write(path, content).await
        .map_err(|e| anyhow!("Failed to write {}: {}", path, e))?;

    Ok(json!({
        "path": path,
        "bytes_written": content.len(),
        "success": true
    }))
}

async fn exec_file_edit(input: &Value) -> Result<Value> {
    let path = input["path"].as_str().ok_or_else(|| anyhow!("path required"))?;
    let old_text = input["old_text"].as_str().ok_or_else(|| anyhow!("old_text required"))?;
    let new_text = input["new_text"].as_str().ok_or_else(|| anyhow!("new_text required"))?;

    let content = tokio::fs::read_to_string(path).await
        .map_err(|e| anyhow!("Failed to read {}: {}", path, e))?;

    let count = content.matches(old_text).count();
    if count == 0 {
        return Err(anyhow!("old_text not found in {}", path));
    }
    if count > 1 {
        return Err(anyhow!("old_text matches {} times in {} — must be unique", count, path));
    }

    let new_content = content.replace(old_text, new_text);
    tokio::fs::write(path, &new_content).await?;

    Ok(json!({ "path": path, "success": true, "replacements": 1 }))
}

async fn exec_file_list(input: &Value) -> Result<Value> {
    let path = input["path"].as_str().ok_or_else(|| anyhow!("path required"))?;
    let recursive = input["recursive"].as_bool().unwrap_or(false);

    let cmd = if recursive {
        format!("find {} -maxdepth 4 | sort | head -200", path)
    } else {
        format!("ls -la {}", path)
    };

    let output = Command::new("bash").arg("-c").arg(&cmd)
        .output().await?;

    Ok(json!({
        "listing": String::from_utf8_lossy(&output.stdout).to_string(),
        "path": path
    }))
}

// ── Git ───────────────────────────────────────────────────────────────────────

async fn exec_git(input: &Value, default_cmd: &str) -> Result<Value> {
    let repo_path = input["repo_path"].as_str().ok_or_else(|| anyhow!("repo_path required"))?;
    let subcmd = input["command"].as_str().unwrap_or(default_cmd);

    let git_cmd = match subcmd {
        "status" => "git status",
        "diff"   => "git diff",
        "log"    => "git log --oneline -20",
        "branch" => "git branch -a",
        "show"   => "git show --stat HEAD",
        other    => return Err(anyhow!("Unknown git subcommand: {}", other)),
    };

    let output = Command::new("bash")
        .arg("-c").arg(git_cmd)
        .current_dir(repo_path)
        .output().await?;

    Ok(json!({
        "output": String::from_utf8_lossy(&output.stdout).to_string(),
        "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
        "exit_code": output.status.code().unwrap_or(-1)
    }))
}

async fn exec_git_commit(input: &Value) -> Result<Value> {
    let repo_path = input["repo_path"].as_str().ok_or_else(|| anyhow!("repo_path required"))?;
    let message = input["message"].as_str().ok_or_else(|| anyhow!("message required"))?;
    let push = input["push"].as_bool().unwrap_or(false);

    let mut cmds = format!("git add -A && git commit -m '{}'", message.replace('\'', "\\'"));
    if push { cmds.push_str(" && git push"); }

    let output = Command::new("bash")
        .arg("-c").arg(&cmds)
        .current_dir(repo_path)
        .output().await?;

    Ok(json!({
        "output": String::from_utf8_lossy(&output.stdout).to_string(),
        "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
        "exit_code": output.status.code().unwrap_or(-1),
        "pushed": push
    }))
}

// ── Web ───────────────────────────────────────────────────────────────────────

async fn exec_web_search(input: &Value, brave_key: Option<&str>) -> Result<Value> {
    let query = input["query"].as_str().ok_or_else(|| anyhow!("query required"))?;
    let count = input["count"].as_u64().unwrap_or(5).min(10);

    let key = brave_key.ok_or_else(|| anyhow!("BRAVE_API_KEY not configured"))?;

    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("Accept", "application/json")
        .header("X-Subscription-Token", key)
        .query(&[("q", query), ("count", &count.to_string())])
        .send().await?
        .json::<Value>().await?;

    let results: Vec<Value> = resp["web"]["results"]
        .as_array()
        .map(|arr| arr.iter().map(|r| json!({
            "title": r["title"],
            "url": r["url"],
            "description": r["description"],
        })).collect())
        .unwrap_or_default();

    Ok(json!({ "query": query, "results": results, "count": results.len() }))
}

async fn exec_web_fetch(input: &Value) -> Result<Value> {
    let url = input["url"].as_str().ok_or_else(|| anyhow!("url required"))?;
    let max_chars = input["max_chars"].as_u64().unwrap_or(8000) as usize;

    let client = reqwest::Client::builder()
        .user_agent("FrankOS/1.0")
        .build()?;

    let resp = client.get(url).send().await?;
    let status = resp.status().as_u16();
    let body = resp.text().await?;

    // Strip HTML tags naively
    let text = strip_html(&body);
    let truncated = if text.len() > max_chars { &text[..max_chars] } else { &text };

    Ok(json!({
        "url": url,
        "status": status,
        "content": truncated,
        "truncated": text.len() > max_chars,
        "total_chars": text.len()
    }))
}

fn strip_html(html: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(c),
            _ => {}
        }
    }
    // Collapse whitespace
    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ── Memory ────────────────────────────────────────────────────────────────────

async fn exec_memory_write(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let title = input["title"].as_str().ok_or_else(|| anyhow!("title required"))?;
    let content = input["content"].as_str().ok_or_else(|| anyhow!("content required"))?;
    let memory_type = input["memory_type"].as_str().unwrap_or("concept");
    let importance = input["importance"].as_i64().unwrap_or(5) as i32;
    let bucket = input["bucket"].as_str().unwrap_or(&ctx.chat_bucket);
    let tags: Vec<String> = input["tags"].as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let memory_id = crate::memory::store(
        &ctx.db,
        bucket,
        "chuck_frank",
        memory_type,
        title,
        content,
        importance,
        &tags,
        None,
        None,
        Some(ctx.session_id),
        "tool_write",
    ).await?;

    // Emit system event
    crate::system_events::emit_memory_write(&ctx.db, ctx.user_id, memory_id, memory_type, title).await;

    Ok(json!({ "stored": true, "title": title, "bucket": bucket, "memory_type": memory_type }))
}

async fn exec_memory_search(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let query = input["query"].as_str().ok_or_else(|| anyhow!("query required"))?;
    let limit = input["limit"].as_u64().unwrap_or(10) as i64;
    let bucket = input["bucket"].as_str().unwrap_or("all");
    use sqlx::Row;

    // Build per-term ILIKE conditions — any term in title or content is a match
    let terms: Vec<String> = query.split_whitespace()
        .filter(|t| t.len() >= 3)
        .map(|t| format!("%{}%", t.to_lowercase()))
        .collect();
    let pattern = if terms.is_empty() { format!("%{}%", query) } else { terms.join("|") };
    // Use full phrase as single pattern when no individual terms
    let like_pat = if terms.is_empty() { format!("%{}%", query) } else { format!("%{}%", query.split_whitespace().next().unwrap_or(query)) };

    // Fetch relevant memories then filter in Rust for multi-term OR matching
    let all_rows = if bucket == "all" {
        sqlx::query(
            "SELECT id, bucket, title, content, memory_type, importance \
             FROM frankos_memory WHERE namespace = 'chuck_frank' AND is_active = true \
             ORDER BY importance DESC, created_at DESC LIMIT 500"
        ).fetch_all(&ctx.db).await?
    } else {
        sqlx::query(
            "SELECT id, bucket, title, content, memory_type, importance \
             FROM frankos_memory WHERE namespace = 'chuck_frank' AND bucket = $1 AND is_active = true \
             ORDER BY importance DESC, created_at DESC LIMIT 500"
        ).bind(bucket).fetch_all(&ctx.db).await?
    };

    let search_terms: Vec<String> = query.split_whitespace()
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_lowercase())
        .collect();
    let fallback = vec![query.to_lowercase()];
    let terms_to_match = if search_terms.is_empty() { &fallback } else { &search_terms };

    let memories: Vec<Value> = all_rows.iter()
        .filter(|r| {
            let title = r.try_get::<String, _>("title").unwrap_or_default().to_lowercase();
            let content = r.try_get::<String, _>("content").unwrap_or_default().to_lowercase();
            terms_to_match.iter().any(|t| title.contains(t.as_str()) || content.contains(t.as_str()))
        })
        .take(limit as usize)
        .map(|r| json!({
            "id": r.try_get::<uuid::Uuid, _>("id").map(|u| u.to_string()).unwrap_or_default(),
            "title": r.try_get::<String, _>("title").unwrap_or_default(),
            "content": r.try_get::<String, _>("content").unwrap_or_default(),
            "memory_type": r.try_get::<String, _>("memory_type").unwrap_or_default(),
            "bucket": r.try_get::<String, _>("bucket").unwrap_or_default(),
            "importance": r.try_get::<i32, _>("importance").unwrap_or(0),
        }))
        .collect();

    let note = if memories.is_empty() {
        "No matching memories found. If the user asked you to remember something, use memory_write now."
    } else { "Results found." };

    Ok(json!({ "query": query, "bucket": bucket, "results": memories, "count": memories.len(), "note": note }))
}

async fn exec_memory_search_semantic(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let query = input["query"].as_str().ok_or_else(|| anyhow!("query required"))?;
    let limit = input["limit"].as_u64().unwrap_or(5).min(20) as i32;
    let namespace = input["namespace"].as_str().unwrap_or("chuck_frank");
    let threshold = input["threshold"].as_f64().map(|t| t as f32);

    let openai_key = ctx.openai_api_key.as_ref()
        .ok_or_else(|| anyhow!("OpenAI API key not configured"))?;

    let results = crate::semantic_search::semantic_search(
        &ctx.db,
        query,
        namespace,
        openai_key,
        limit,
        threshold,
    ).await?;

    let memories: Vec<Value> = results.iter()
        .map(|r| json!({
            "id": r.id,
            "title": r.title,
            "content": r.content,
            "memory_type": r.memory_type,
            "bucket": r.bucket,
            "importance": r.importance,
            "similarity": r.similarity,
            "tags": r.tags,
        }))
        .collect();

    let note = if memories.is_empty() {
        "No semantically similar memories found. Try lowering the threshold or use keyword search."
    } else {
        "Semantic search results ranked by similarity."
    };

    Ok(json!({
        "query": query,
        "namespace": namespace,
        "results": memories,
        "count": memories.len(),
        "note": note
    }))
}

async fn exec_memory_list(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let bucket = input["bucket"].as_str().unwrap_or("all");
    let memory_type_filter = input["memory_type"].as_str();
    let tag_filter = input["tag"].as_str();
    let limit = input["limit"].as_u64().unwrap_or(20) as i64;
    use sqlx::Row;

    let rows = sqlx::query(
        r#"SELECT id, bucket, title, content, memory_type, importance, tags, created_at
           FROM frankos_memory WHERE namespace = 'chuck_frank'
           AND ($1 = 'all' OR bucket = $1)
           AND ($2::text IS NULL OR memory_type = $2)
           AND ($3::text IS NULL OR $3 = ANY(tags))
           ORDER BY importance DESC, created_at DESC LIMIT $4"#
    )
    .bind(bucket).bind(memory_type_filter).bind(tag_filter).bind(limit)
    .fetch_all(&ctx.db).await?;

    let memories: Vec<Value> = rows.iter().map(|r| json!({
        "id": r.try_get::<uuid::Uuid, _>("id").map(|u| u.to_string()).unwrap_or_default(),
        "title": r.try_get::<String, _>("title").unwrap_or_default(),
        "content": r.try_get::<String, _>("content").unwrap_or_default(),
        "memory_type": r.try_get::<String, _>("memory_type").unwrap_or_default(),
        "bucket": r.try_get::<String, _>("bucket").unwrap_or_default(),
        "importance": r.try_get::<i32, _>("importance").unwrap_or(0),
        "tags": r.try_get::<Vec<String>, _>("tags").unwrap_or_default(),
    })).collect();

    Ok(json!({ "bucket": bucket, "count": memories.len(), "memories": memories }))
}

async fn exec_memory_update(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let title = input["title"].as_str().ok_or_else(|| anyhow!("title required"))?;
    use sqlx::Row;

    let row = sqlx::query(
        "SELECT id, content, importance, tags FROM frankos_memory WHERE namespace = 'chuck_frank' AND title ILIKE $1 LIMIT 1"
    ).bind(title).fetch_optional(&ctx.db).await?;

    let Some(row) = row else {
        return Ok(json!({ "updated": false, "error": format!("No memory found: {}", title) }));
    };

    let id: uuid::Uuid = row.try_get("id")?;
    let cur_content: String = row.try_get("content").unwrap_or_default();
    let cur_importance: i32 = row.try_get("importance").unwrap_or(5);
    let cur_tags: Vec<String> = row.try_get("tags").unwrap_or_default();

    let new_content = input["content"].as_str().unwrap_or(&cur_content);
    let new_importance = input["importance"].as_i64().unwrap_or(cur_importance as i64) as i32;
    let new_tags: Vec<String> = input["tags"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or(cur_tags);

    sqlx::query(
        "UPDATE frankos_memory SET content = $1, importance = $2, tags = $3, updated_at = NOW() WHERE id = $4"
    ).bind(new_content).bind(new_importance).bind(&new_tags).bind(id).execute(&ctx.db).await?;

    Ok(json!({ "updated": true, "title": title, "importance": new_importance }))
}

async fn exec_memory_delete(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let title = input["title"].as_str().ok_or_else(|| anyhow!("title required"))?;
    let bucket = input["bucket"].as_str();

    let result = if let Some(b) = bucket {
        sqlx::query("DELETE FROM frankos_memory WHERE namespace = 'chuck_frank' AND title ILIKE $1 AND bucket = $2")
            .bind(title).bind(b).execute(&ctx.db).await?
    } else {
        sqlx::query("DELETE FROM frankos_memory WHERE namespace = 'chuck_frank' AND title ILIKE $1")
            .bind(title).execute(&ctx.db).await?
    };

    Ok(json!({ "deleted": result.rows_affected() > 0, "rows": result.rows_affected(), "title": title }))
}

async fn exec_memory_move(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let title = input["title"].as_str().ok_or_else(|| anyhow!("title required"))?;
    let to_bucket = input["to_bucket"].as_str().ok_or_else(|| anyhow!("to_bucket required"))?;
    let valid = ["personal", "work", "training", "identity", "personal_telos", "personal_work"];
    if !valid.contains(&to_bucket) {
        return Ok(json!({ "moved": false, "error": format!("Invalid bucket: {}", to_bucket) }));
    }
    let result = sqlx::query(
        "UPDATE frankos_memory SET bucket = $1, updated_at = NOW() WHERE namespace = 'chuck_frank' AND title ILIKE $2"
    ).bind(to_bucket).bind(title).execute(&ctx.db).await?;

    Ok(json!({ "moved": result.rows_affected() > 0, "title": title, "to_bucket": to_bucket }))
}

// ── Service Control ───────────────────────────────────────────────────────────

async fn exec_service_ctl(input: &Value) -> Result<Value> {
    let service = input["service"].as_str().ok_or_else(|| anyhow!("service required"))?;
    let action = input["action"].as_str().ok_or_else(|| anyhow!("action required"))?;

    // Safety: only allow known actions
    let allowed = ["status", "start", "stop", "restart", "enable", "disable"];
    if !allowed.contains(&action) {
        return Err(anyhow!("Action '{}' not allowed. Use: {}", action, allowed.join(", ")));
    }

    // CRITICAL SAFETY: never stop or restart frankos-gateway from within itself.
    // Stopping the service kills this running process before restart completes — total system failure.
    // Deployments must be performed externally by Mac Frank.
    let destructive_actions = ["stop", "restart", "disable"];
    if service == "frankos-gateway" && destructive_actions.contains(&action) {
        return Ok(json!({
            "success": false,
            "error": "SAFETY BLOCK: frankos-gateway cannot stop or restart itself. Write READY_FOR_DEPLOYMENT to FRANK_TO_MAC.md — Mac Frank deploys externally.",
            "action": action,
            "service": service
        }));
    }

    let output = Command::new("systemctl")
        .arg(action)
        .arg(service)
        .output().await?;

    Ok(json!({
        "service": service,
        "action": action,
        "output": String::from_utf8_lossy(&output.stdout).to_string(),
        "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
        "exit_code": output.status.code().unwrap_or(-1)
    }))
}

// ── Cargo Build ───────────────────────────────────────────────────────────────

async fn exec_cargo_build(input: &Value) -> Result<Value> {
    let path = input["path"].as_str().ok_or_else(|| anyhow!("path required"))?;
    let release = input["release"].as_bool().unwrap_or(true);

    let mut args = vec!["build".to_string()];
    if release { args.push("--release".to_string()); }
    if let Some(bin) = input["bin"].as_str() {
        args.push("--bin".to_string());
        args.push(bin.to_string());
    }

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        Command::new("cargo")
            .args(&args)
            .current_dir(path)
            .env("CARGO_TERM_COLOR", "never")
            .output()
    ).await
    .map_err(|_| anyhow!("cargo build timed out after 300s"))??;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let success = output.status.success();

    Ok(json!({
        "success": success,
        "stdout": stdout,
        "stderr": stderr,
        "exit_code": output.status.code().unwrap_or(-1)
    }))
}

// ── Process List ──────────────────────────────────────────────────────────────

async fn exec_process_list(input: &Value) -> Result<Value> {
    let filter = input["filter"].as_str().unwrap_or("");
    let cmd = if filter.is_empty() {
        "ps aux --sort=-%cpu | head -30".to_string()
    } else {
        format!("ps aux | grep -i '{}' | grep -v grep", filter)
    };

    let output = Command::new("bash").arg("-c").arg(&cmd).output().await?;
    Ok(json!({
        "processes": String::from_utf8_lossy(&output.stdout).to_string(),
        "filter": filter
    }))
}

// ── Spawn Agent (stub — delegates to agents module) ──────────────────────────

async fn exec_spawn_agent(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let name = input["name"].as_str().unwrap_or("worker").to_string();
    let goal = input["goal"].as_str().ok_or_else(|| anyhow!("goal required"))?.to_string();
    let model = input["model"].as_str().unwrap_or("haiku").to_string();
    let tools: Vec<String> = input["tools"].as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();

    // Create agent record — NULL parent_session_id when called from agent context (nil UUID = FK violation)
    let parent_sid: Option<uuid::Uuid> = if ctx.session_id == uuid::Uuid::nil() { None } else { Some(ctx.session_id) };
    let agent_id: uuid::Uuid = sqlx::query_scalar(
        r#"INSERT INTO frankos_agents (name, goal, status, tools_allowed, model, parent_session_id, user_id)
           VALUES ($1, $2, 'spawned', $3, $4, $5, $6) RETURNING id"#
    )
    .bind(&name)
    .bind(&goal)
    .bind(serde_json::to_value(&tools).unwrap_or(json!([])))
    .bind(&model)
    .bind(parent_sid)
    .bind(ctx.user_id)
    .fetch_one(&ctx.db).await?;

    info!("Spawned agent {} ({}): {}", name, agent_id, goal);

    Ok(json!({
        "agent_id": agent_id.to_string(),
        "name": name,
        "goal": goal,
        "status": "pending",
        "message": "Agent spawned. Check /api/v1/agents for status."
    }))
}

// all_tools_with_google replaced by all_tools_full below

// ── Google AI tool executors ──────────────────────────────────────────────────

fn make_google_client(ctx: &ToolContext) -> crate::google_ai::GoogleAiClient {
    crate::google_ai::GoogleAiClient::new(
        ctx.google_ai_key.clone(),
        ctx.google_ai_project.clone(),
    )
}

async fn exec_generate_image(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let prompt       = input["prompt"].as_str().ok_or_else(|| anyhow!("prompt required"))?;
    let aspect_ratio = input["aspect_ratio"].as_str().unwrap_or("1:1");
    let count        = input["count"].as_u64().unwrap_or(1) as u32;
    let client       = make_google_client(ctx);
    let output_dir   = "/opt/frankos/workspace/generated/images";

    let paths = client.generate_image(prompt, aspect_ratio, count, output_dir).await?;
    // Convert server paths to public URLs
    let urls: Vec<String> = paths.iter().map(|p| {
        let filename = p.trim_start_matches("/opt/frankos/workspace/generated/");
        format!("https://frank.swarmlogic.cloud/files/{}", filename)
    }).collect();
    Ok(json!({
        "success": true,
        "urls": urls,
        "count": urls.len(),
        "prompt": prompt,
        "display_hint": "Embed each image in your reply using markdown: ![image description](url)",
    }))
}

async fn exec_generate_video(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let prompt    = input["prompt"].as_str().ok_or_else(|| anyhow!("prompt required"))?;
    let duration  = input["duration_seconds"].as_u64().unwrap_or(5) as u32;
    let ratio     = input["aspect_ratio"].as_str().unwrap_or("16:9");
    let client    = make_google_client(ctx);
    let output_dir = "/opt/frankos/workspace/generated/videos";

    let path = client.generate_video(prompt, duration, ratio, output_dir).await?;
    Ok(json!({
        "success": true,
        "file": path,
        "duration_seconds": duration,
        "prompt": prompt,
    }))
}

async fn exec_analyze_image(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let url      = input["image_url"].as_str().ok_or_else(|| anyhow!("image_url required"))?;
    let question = input["question"].as_str().unwrap_or("");
    let client   = make_google_client(ctx);

    let result = client.analyze_image(url, question).await?;
    Ok(json!({ "analysis": result, "image_url": url }))
}

async fn exec_gemini_research(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let prompt  = input["prompt"].as_str().ok_or_else(|| anyhow!("prompt required"))?;
    let context = input["context"].as_str().unwrap_or("");
    let client  = make_google_client(ctx);

    let result = client.gemini_research(prompt, context).await?;
    Ok(json!({ "result": result, "model": "gemini-1.5-pro" }))
}

async fn exec_gemini_chat(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let prompt = input["prompt"].as_str().ok_or_else(|| anyhow!("prompt required"))?;
    let model  = input["model"].as_str().unwrap_or("gemini-1.5-flash");
    let client = make_google_client(ctx);

    let result = client.gemini_chat(prompt, model, None).await?;
    Ok(json!({ "result": result, "model": model }))
}

// ── Luma tool executors ───────────────────────────────────────────────────────

// make_luma_client defined below

/// All tools including Google AI and Luma
pub fn all_tools_full() -> Vec<ToolDef> {
    let mut tools = all_tools();
    tools.extend(crate::google_ai::google_ai_tools());
    tools.extend(crate::luma::luma_tools());
    tools
}

fn make_luma_client(ctx: &ToolContext) -> crate::luma::LumaClient {
    crate::luma::LumaClient::new(ctx.luma_api_key.clone())
}

async fn exec_luma_text_to_video(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let prompt       = input["prompt"].as_str().ok_or_else(|| anyhow!("prompt required"))?;
    let model        = input["model"].as_str().unwrap_or("ray-2");
    let resolution   = input["resolution"].as_str().unwrap_or("720p");
    let duration     = input["duration"].as_str().unwrap_or("5s");
    let aspect_ratio = input["aspect_ratio"].as_str().unwrap_or("16:9");
    let loop_video   = input["loop"].as_bool().unwrap_or(false);
    let concepts: Vec<String> = input["concepts"].as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let output_dir = "/opt/frankos/workspace/generated/videos";
    let gen = make_luma_client(ctx)
        .text_to_video(prompt, model, resolution, duration, aspect_ratio, loop_video, concepts, output_dir)
        .await?;
    let vid_url = gen.download_url.as_deref().map(|p| {
        let f = p.trim_start_matches("/opt/frankos/workspace/generated/");
        format!("https://frank.swarmlogic.cloud/files/{}", f)
    }).unwrap_or_default();
    Ok(json!({"success": true, "generation_id": gen.id, "urls": [vid_url], "prompt": prompt, "display_hint": "Share the video link with Chuck"}))
}

async fn exec_luma_image_to_video(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let prompt       = input["prompt"].as_str().ok_or_else(|| anyhow!("prompt required"))?;
    let start_url    = input["start_image_url"].as_str().ok_or_else(|| anyhow!("start_image_url required"))?;
    let end_url      = input["end_image_url"].as_str();
    let model        = input["model"].as_str().unwrap_or("ray-2");
    let aspect_ratio = input["aspect_ratio"].as_str().unwrap_or("16:9");
    let loop_video   = input["loop"].as_bool().unwrap_or(false);
    let output_dir   = "/opt/frankos/workspace/generated/videos";
    let gen = make_luma_client(ctx)
        .image_to_video(prompt, start_url, end_url, model, aspect_ratio, loop_video, output_dir)
        .await?;
    let vid_url = gen.download_url.as_deref().map(|p| { let f = p.trim_start_matches("/opt/frankos/workspace/generated/"); format!("https://frank.swarmlogic.cloud/files/{}", f) }).unwrap_or_default();
    Ok(json!({"success": true, "generation_id": gen.id, "urls": [vid_url]}))
}

async fn exec_luma_text_to_image(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let prompt       = input["prompt"].as_str().ok_or_else(|| anyhow!("prompt required"))?;
    let model        = input["model"].as_str().unwrap_or("photon-1");
    let aspect_ratio = input["aspect_ratio"].as_str().unwrap_or("16:9");
    let output_dir   = "/opt/frankos/workspace/generated/images";
    let path = make_luma_client(ctx)
        .text_to_image(prompt, model, aspect_ratio, output_dir)
        .await?;
    let img_url = { let f = path.trim_start_matches("/opt/frankos/workspace/generated/"); format!("https://frank.swarmlogic.cloud/files/{}", f) };
    Ok(json!({"success": true, "urls": [img_url], "prompt": prompt, "display_hint": "Embed image: ![description](url)"}))
}

async fn exec_luma_image_reference(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let prompt   = input["prompt"].as_str().ok_or_else(|| anyhow!("prompt required"))?;
    let refs: Vec<&str> = input["reference_urls"].as_array()
        .ok_or_else(|| anyhow!("reference_urls required"))?
        .iter().filter_map(|v| v.as_str()).collect();
    let weight       = input["weight"].as_f64().unwrap_or(0.85) as f32;
    let model        = input["model"].as_str().unwrap_or("photon-1");
    let aspect_ratio = input["aspect_ratio"].as_str().unwrap_or("16:9");
    let output_dir   = "/opt/frankos/workspace/generated/images";
    let path = make_luma_client(ctx)
        .image_reference(prompt, refs, weight, model, aspect_ratio, output_dir)
        .await?;
    let img_url = { let f = path.trim_start_matches("/opt/frankos/workspace/generated/"); format!("https://frank.swarmlogic.cloud/files/{}", f) };
    Ok(json!({"success": true, "urls": [img_url], "prompt": prompt}))
}

async fn exec_luma_style_reference(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let prompt     = input["prompt"].as_str().ok_or_else(|| anyhow!("prompt required"))?;
    let style_url  = input["style_image_url"].as_str().ok_or_else(|| anyhow!("style_image_url required"))?;
    let weight     = input["weight"].as_f64().unwrap_or(0.8) as f32;
    let model      = input["model"].as_str().unwrap_or("photon-1");
    let aspect_r   = input["aspect_ratio"].as_str().unwrap_or("16:9");
    let output_dir = "/opt/frankos/workspace/generated/images";
    let path = make_luma_client(ctx)
        .style_reference(prompt, style_url, weight, model, aspect_r, output_dir)
        .await?;
    let img_url = { let f = path.trim_start_matches("/opt/frankos/workspace/generated/"); format!("https://frank.swarmlogic.cloud/files/{}", f) };
    Ok(json!({"success": true, "urls": [img_url], "prompt": prompt}))
}

async fn exec_luma_list_concepts(ctx: &ToolContext) -> Result<Value> {
    let concepts = make_luma_client(ctx).list_concepts().await?;
    let count = concepts.len();
    Ok(json!({ "concepts": concepts, "count": count }))
}

async fn exec_luma_list_generations(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let limit = input["limit"].as_u64().unwrap_or(10) as u32;
    let gens = make_luma_client(ctx).list_generations(limit).await?;
    let count = gens.len();
    Ok(json!({ "generations": gens, "count": count }))
}

// ═══════════════════════════════════════════════════════════════════════════
// v3 TOOL DEFINITIONS — Forge + Nexus + Swarm
// ═══════════════════════════════════════════════════════════════════════════

pub fn v3_tools() -> Vec<ToolDef> {
    vec![
        // ── Forge: process management ──
        ToolDef {
            name: "process_spawn".into(),
            description: "Spawn a background process. Returns immediately with a process_id. The process runs async — use process_status, process_log, process_wait to interact. Perfect for long builds, servers, test runners.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to run" },
                    "cwd": { "type": "string", "description": "Working directory (default: /opt/frankos/workspace)" },
                    "timeout_secs": { "type": "integer", "description": "Auto-kill after N seconds (0 = no timeout)" },
                    "env": { "type": "object", "description": "Extra environment variables" }
                },
                "required": ["command"]
            }),
        },
        ToolDef {
            name: "process_status".into(),
            description: "Check the status of a Forge process. Returns running/exited/killed/timed_out and exit code.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "process_id": { "type": "string", "description": "UUID from process_spawn" }
                },
                "required": ["process_id"]
            }),
        },
        ToolDef {
            name: "process_log".into(),
            description: "Read the last N lines of a process's stdout/stderr ring buffer.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "process_id": { "type": "string" },
                    "lines": { "type": "integer", "description": "Lines to return (default 50, max 500)" }
                },
                "required": ["process_id"]
            }),
        },
        ToolDef {
            name: "process_write".into(),
            description: "Write data to a process's stdin. Used to answer prompts or control interactive processes.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "process_id": { "type": "string" },
                    "data": { "type": "string", "description": "Data to write (newline appended if missing)" }
                },
                "required": ["process_id", "data"]
            }),
        },
        ToolDef {
            name: "process_kill".into(),
            description: "Send a kill signal to a Forge process.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "process_id": { "type": "string" }
                },
                "required": ["process_id"]
            }),
        },
        ToolDef {
            name: "process_wait".into(),
            description: "Block until a process exits (or timeout). Returns final status and last 30 lines of output. Use after process_spawn for builds you need to validate.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "process_id": { "type": "string" },
                    "timeout_secs": { "type": "integer", "description": "Max seconds to wait (default 300)" }
                },
                "required": ["process_id"]
            }),
        },
        ToolDef {
            name: "forge_list".into(),
            description: "List all active Forge-managed background processes.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        // ── Nexus: scheduling ──
        ToolDef {
            name: "schedule_task".into(),
            description: "Schedule a task to run at a future time, on a cron schedule, or at an interval. Frank will execute it autonomously. Use for reminders, recurring checks, timed deployments.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Human-readable name for this trigger" },
                    "schedule": {
                        "type": "object",
                        "description": "One of: {\"type\":\"once\",\"at\":\"2026-08-08T09:00:00Z\"} or {\"type\":\"cron\",\"expr\":\"0 9 * * 1-5\"} or {\"type\":\"interval_ms\",\"ms\":3600000}"
                    },
                    "payload": {
                        "type": "object",
                        "description": "One of: {\"type\":\"agent_turn\",\"prompt\":\"...\"} or {\"type\":\"notify\",\"title\":\"...\",\"body\":\"...\"}"
                    },
                    "max_fires": { "type": "integer", "description": "Max times to fire (0 = unlimited, 1 = one-shot)" }
                },
                "required": ["name", "schedule", "payload"]
            }),
        },
        ToolDef {
            name: "list_triggers".into(),
            description: "List all active scheduled triggers.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
        ToolDef {
            name: "delete_trigger".into(),
            description: "Delete a scheduled trigger by ID.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "trigger_id": { "type": "string" }
                },
                "required": ["trigger_id"]
            }),
        },
        // ── apply_patch ──
        ToolDef {
            name: "apply_patch".into(),
            description: "Apply a multi-file patch atomically. Uses *** Begin Patch / *** End Patch format with *** path/to/file *** section headers and unified diff hunks. All files validated before any are written — all or nothing.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "patch": { "type": "string", "description": "Full patch text from *** Begin Patch to *** End Patch" }
                },
                "required": ["patch"]
            }),
        },
        ToolDef {
            name: "tool_pipeline".into(),
            description: "Execute a sequence of tools without LLM round-trips. Each step runs using execute_tool() directly. Stops on first failure. Use for chaining operations like build → test → deploy.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "description": "Array of tool steps to execute in sequence",
                        "items": {
                            "type": "object",
                            "properties": {
                                "tool": { "type": "string", "description": "Tool name to execute" },
                                "input": { "type": "object", "description": "Input parameters for the tool" }
                            },
                            "required": ["tool", "input"]
                        }
                    }
                },
                "required": ["steps"]
            }),
        },
    ]
}

/// Gap 10A: Compound Internal Tools — encapsulate multi-step workflows
pub fn compound_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "build_and_deploy".into(),
            description: "Build a Rust service with cargo and deploy it atomically. Compiles, stops service, copies binary, restarts, and validates health. Returns build output and service status.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "service": { "type": "string", "description": "Service name (e.g. frankos-gateway)" },
                    "release": { "type": "boolean", "description": "Build in release mode (default true)" }
                },
                "required": ["service"]
            }),
        },
        ToolDef {
            name: "db_migration".into(),
            description: "Apply a database migration transactionally. Tracks applied migrations, skips if already run, rolls back on error.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Migration name (e.g. v9_wave_context)" },
                    "sql": { "type": "string", "description": "SQL to execute" },
                    "rollback_sql": { "type": "string", "description": "Optional rollback SQL" }
                },
                "required": ["name", "sql"]
            }),
        },
        ToolDef {
            name: "agent_spawn".into(),
            description: "Spawn a new agent with proper session context. Fixes FK constraint by auto-filling parent_session_id and triggered_by from current context.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Agent name (e.g. Engineer, Researcher)" },
                    "goal": { "type": "string", "description": "What this agent should accomplish" },
                    "model": { "type": "string", "description": "Model: claude-haiku-4, claude-sonnet-4-5, claude-opus-4" },
                    "context": { "type": "string", "description": "Additional context or instructions" }
                },
                "required": ["name", "goal"]
            }),
        },
        ToolDef {
            name: "memory_commit".into(),
            description: "Batch-write multiple memory entries atomically. Deduplicates by title, updates if importance is higher. Max 10 entries per call.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "entries": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": { "type": "string" },
                                "content": { "type": "string" },
                                "memory_type": { "type": "string" },
                                "tags": { "type": "array", "items": { "type": "string" } },
                                "importance": { "type": "integer" }
                            },
                            "required": ["title", "content"]
                        }
                    }
                },
                "required": ["entries"]
            }),
        },
        ToolDef {
            name: "mailbox_write".into(),
            description: "Write a message to the agent mailbox. Used to escalate BLOCKED conditions or send solutions between agents.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "to_agent_id": { "type": "string", "description": "UUID of recipient agent, or null for SuperFrank" },
                    "message_type": { "type": "string", "description": "blocked | solution | escalation | info" },
                    "subject": { "type": "string", "description": "Short subject line" },
                    "content": { "type": "string", "description": "Full message content with context" }
                },
                "required": ["message_type", "subject", "content"]
            }),
        },
        ToolDef {
            name: "mailbox_read".into(),
            description: "Read unread messages from the agent mailbox. Returns messages addressed to this agent.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "to_agent_id": { "type": "string", "description": "UUID of recipient (null = SuperFrank), defaults to current agent" },
                    "status": { "type": "string", "description": "Filter by status: unread (default) | read | actioned" },
                    "limit": { "type": "integer", "description": "Max messages to return (default 10)" }
                },
                "required": []
            }),
        },
        ToolDef {
            name: "mailbox_mark_read".into(),
            description: "Mark mailbox messages as read after processing them.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "mailbox_ids": { "type": "array", "items": { "type": "string" }, "description": "Array of mailbox message UUIDs to mark read" }
                },
                "required": ["mailbox_ids"]
            }),
        },

        ToolDef {
            name: "task_list_pending".into(),
            description: "List all PENDING tasks assigned to Engineer. Returns task_id, title, description, priority. Call this first to find what to work on.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDef {
            name: "task_claim".into(),
            description: "Atomically claim a PENDING task. Sets status to IN_PROGRESS and returns full task details. Always claim before starting work.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "UUID of the task to claim" }
                },
                "required": ["task_id"]
            }),
        },
        ToolDef {
            name: "task_done".into(),
            description: "Mark a task COMPLETE. Call this after successfully finishing work on a claimed task.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "UUID of the task" },
                    "outcome": { "type": "string", "description": "Short summary of what was done" }
                },
                "required": ["task_id", "outcome"]
            }),
        },
        ToolDef {
            name: "task_block".into(),
            description: "Mark a task BLOCKED. Use when you cannot complete it without external help. Writes blocker to FRANK_TO_MAC.md for Mac Frank.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "UUID of the task" },
                    "reason": { "type": "string", "description": "Exact reason blocked — what is needed to unblock" }
                },
                "required": ["task_id", "reason"]
            }),
        },
        ToolDef {
            name: "notify_internal".into(),
            description: "Write an internal SuperFrank notification to frank_notifications (no external email dependency). Use for progress, blockers, and handoff signals.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short notification title" },
                    "body": { "type": "string", "description": "Detailed notification text" },
                    "level": { "type": "string", "description": "info | warning | blocker | success (default info)" },
                    "source": { "type": "string", "description": "Source label (default agent)" },
                    "target_user_id": { "type": "string", "description": "Optional user UUID. Defaults to current user." },
                    "metadata": { "type": "object", "description": "Optional JSON metadata payload" }
                },
                "required": ["title", "body"]
            }),
        },
        ToolDef {
            name: "notification_inbox".into(),
            description: "Read internal notifications from frank_notifications for the current user.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string", "description": "queued | delivered | acknowledged (default queued)" },
                    "source": { "type": "string", "description": "Optional source filter" },
                    "limit": { "type": "integer", "description": "Max notifications (default 20, max 100)" }
                }
            }),
        },
        ToolDef {
            name: "notification_ack".into(),
            description: "Acknowledge one or more internal notifications.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "notification_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Array of notification UUIDs"
                    }
                },
                "required": ["notification_ids"]
            }),
        },
    ]
}

/// All tools including v3 Forge + Nexus + Swarm + Gap 10A Compound Tools
pub fn all_tools_v3() -> Vec<ToolDef> {
    let mut tools = all_tools_full();
    tools.extend(v3_tools());
    tools.extend(goal_tools());
    tools.extend(skill_tools());
    tools.extend(compound_tools());
    tools
}

// ── v3 tool dispatch (called from execute_tool) ────────────────────────────

pub async fn execute_v3_tool(name: &str, input: &Value, ctx: &ToolContext) -> Option<Value> {
    use crate::forge_tools::*;

    match name {
        "process_spawn" => {
            if let Some(forge) = &ctx.forge {
                Some(tool_process_spawn(input, forge).await)
            } else {
                Some(json!({ "error": "Forge not available" }))
            }
        }
        "process_status" => {
            if let Some(forge) = &ctx.forge {
                Some(tool_process_status(input, forge).await)
            } else { Some(json!({ "error": "Forge not available" })) }
        }
        "process_log" => {
            if let Some(forge) = &ctx.forge {
                Some(tool_process_log(input, forge).await)
            } else { Some(json!({ "error": "Forge not available" })) }
        }
        "process_write" => {
            if let Some(forge) = &ctx.forge {
                Some(tool_process_write(input, forge).await)
            } else { Some(json!({ "error": "Forge not available" })) }
        }
        "process_kill" => {
            if let Some(forge) = &ctx.forge {
                Some(tool_process_kill(input, forge).await)
            } else { Some(json!({ "error": "Forge not available" })) }
        }
        "process_wait" => {
            if let Some(forge) = &ctx.forge {
                Some(tool_process_wait(input, forge).await)
            } else { Some(json!({ "error": "Forge not available" })) }
        }
        "forge_list" => {
            if let Some(forge) = &ctx.forge {
                Some(tool_process_list(forge).await)
            } else { Some(json!({ "error": "Forge not available" })) }
        }
        "schedule_task" => Some(tool_schedule_task(input, &ctx.db, ctx.user_id).await),
        "list_triggers"  => Some(tool_list_triggers(&ctx.db, ctx.user_id).await),
        "delete_trigger"  => Some(tool_delete_trigger(input, &ctx.db).await),
        "apply_patch"     => Some(tool_apply_patch(input).await),
        "goal_create"      => Some(crate::goals_tools::exec_goal_create(input, ctx).await.unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}))),
        "goal_update"      => Some(crate::goals_tools::exec_goal_update(input, ctx).await.unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}))),
        "goal_list"        => Some(crate::goals_tools::exec_goal_list(input, ctx).await.unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}))),
        "goal_complete"    => {
            let result = crate::goals_tools::exec_goal_complete(input, ctx).await.unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}));
            // Gap 7D: Auto-memory after goal completion
            if result.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
                if let (Some(title), Some(goal_id_str)) = (result.get("title").and_then(|v| v.as_str()), result.get("goal_id").and_then(|v| v.as_str())) {
                    if let Ok(goal_id) = goal_id_str.parse() {
                        let notes = input["notes"].as_str();
                        let db = ctx.db.clone();
                        let title = title.to_string();
                        let notes = notes.map(String::from);
                        tokio::spawn(async move {
                            crate::auto_memory::after_goal_complete(&db, &title, notes.as_deref(), goal_id).await;
                        });
                    }
                }
            }
            Some(result)
        }
        "plan_set"         => Some(crate::goals_tools::exec_plan_set(input, ctx).await.unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}))),
        "plan_step_update" => {
            let result = crate::goals_tools::exec_plan_step_update(input, ctx).await.unwrap_or_else(|e| serde_json::json!({"error": e.to_string()}));
            // Hook: Auto-continuation after step completion
            if let (Some(goal_id_str), Some(status)) = (result.get("goal_id").and_then(|v| v.as_str()), result.get("status").and_then(|v| v.as_str())) {
                if let Ok(goal_id) = goal_id_str.parse() {
                    let _ = crate::plan_continuation::maybe_auto_continue(&ctx.db, ctx.user_id, goal_id, status).await;
                    
                    // Gap 7D: Auto-memory after step completion
                    if status == "complete" {
                        if let (Some(goal_title), Some(step_title), Some(step_num)) = (
                            result.get("goal_title").and_then(|v| v.as_str()),
                            result.get("step_title").and_then(|v| v.as_str()),
                            result.get("step_number").and_then(|v| v.as_i64())
                        ) {
                            let notes = input["notes"].as_str();
                            let db = ctx.db.clone();
                            let goal_title = goal_title.to_string();
                            let step_title = step_title.to_string();
                            let notes = notes.map(String::from);
                            tokio::spawn(async move {
                                crate::auto_memory::after_step_complete(
                                    &db, &goal_title, &step_title, step_num as i32, notes.as_deref(), goal_id
                                ).await;
                            });
                        }
                    }
                }
            }
            Some(result)
        }
        "tool_pipeline" => Some(crate::forge_tools::exec_tool_pipeline(input, ctx).await),
        _ => None,
    }
}

// ── Goal + Planning tools (Gap 2) ─────────────────────────────────────────────

pub fn goal_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "goal_create".into(),
            description: "Create a new goal. Goals persist across sessions and are injected into the system prompt so Frank always knows what it's working on.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title":       { "type": "string", "description": "Short goal title" },
                    "description": { "type": "string", "description": "Full goal description and intent" },
                    "priority":    { "type": "integer", "description": "1 (low) to 10 (critical), default 5" },
                    "context":     { "type": "object", "description": "Optional metadata (JSON)" }
                },
                "required": ["title", "description"]
            }),
        },
        ToolDef {
            name: "goal_update".into(),
            description: "Update a goal's title, description, status, or priority.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "goal_id":     { "type": "string", "description": "Goal UUID" },
                    "title":       { "type": "string" },
                    "description": { "type": "string" },
                    "status":      { "type": "string", "enum": ["active", "paused", "complete", "cancelled"] },
                    "priority":    { "type": "integer" }
                },
                "required": ["goal_id"]
            }),
        },
        ToolDef {
            name: "goal_list".into(),
            description: "List goals. Defaults to active goals. Pass status to filter.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string", "enum": ["active", "paused", "complete", "cancelled"], "description": "Filter by status (default: active)" }
                }
            }),
        },
        ToolDef {
            name: "goal_complete".into(),
            description: "Mark a goal as complete. Pass optional notes about what was accomplished.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "goal_id": { "type": "string", "description": "Goal UUID" },
                    "notes":   { "type": "string", "description": "Optional completion notes" }
                },
                "required": ["goal_id"]
            }),
        },
        ToolDef {
            name: "plan_set".into(),
            description: "Set (replace) the full step list for a goal. Pass an array of steps with step_number, title, and optional description.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "goal_id": { "type": "string", "description": "Goal UUID" },
                    "steps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "step_number": { "type": "integer" },
                                "title":       { "type": "string" },
                                "description": { "type": "string" }
                            },
                            "required": ["step_number", "title"]
                        }
                    }
                },
                "required": ["goal_id", "steps"]
            }),
        },
        ToolDef {
            name: "plan_step_update".into(),
            description: "Update a single plan step's status and optional notes.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "step_id": { "type": "string", "description": "Step UUID (returned from plan_set)" },
                    "status":  { "type": "string", "enum": ["pending", "in_progress", "complete", "blocked", "skipped"] },
                    "notes":   { "type": "string", "description": "Optional notes" }
                },
                "required": ["step_id", "status"]
            }),
        },
    ]
}

// ── Skills tools (Gap 4) ──────────────────────────────────────────────────────

pub fn skill_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "skill_save".into(),
            description: "Save or update a reusable skill/procedure. Skills persist across sessions and can be recalled by name. Pass name, description, and steps array.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name":        { "type": "string", "description": "Unique skill name (slug format)" },
                    "description": { "type": "string", "description": "What the skill does" },
                    "steps":       { "type": "array",  "description": "Array of step strings or objects", "items": {} },
                    "tags":        { "type": "array",  "description": "Optional tags for filtering", "items": { "type": "string" } }
                },
                "required": ["name", "description", "steps"]
            }),
        },
        ToolDef {
            name: "skill_load".into(),
            description: "Load a skill by name or id. Returns the full skill including steps.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Skill name" },
                    "id":   { "type": "string", "description": "Skill UUID" }
                }
            }),
        },
        ToolDef {
            name: "skill_list".into(),
            description: "List available skills. Optionally filter by tag or search term.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tag":    { "type": "string", "description": "Filter by tag" },
                    "search": { "type": "string", "description": "Search name/description" },
                    "limit":  { "type": "integer", "description": "Max results (default 50)" }
                }
            }),
        },
        ToolDef {
            name: "skill_use".into(),
            description: "Mark a skill as used (increments use_count) and return its full content. Use this when about to execute a skill.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Skill name" }
                },
                "required": ["name"]
            }),
        },
    ]
}

// ── Gap 7D Helper: Get active goal ID for auto-memory tagging ─────────────────

async fn get_active_goal_id(db: &sqlx::PgPool, user_id: uuid::Uuid) -> Option<uuid::Uuid> {
    sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT id FROM frank_goals WHERE user_id = $1 AND status = 'active' ORDER BY priority DESC, created_at DESC LIMIT 1"
    )
    .bind(user_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
}

// ══════════════════════════════════════════════════════════════════════════════
// Gap 10A: Compound Internal Tools — multi-step workflows in a single call
// ══════════════════════════════════════════════════════════════════════════════

/// Tool 1: build_and_deploy — compile Rust service, deploy, and validate
async fn exec_build_and_deploy(input: &Value) -> Result<Value> {
    let service = input["service"].as_str().ok_or_else(|| anyhow!("Missing service"))?;
    let release = input.get("release").and_then(|v| v.as_bool()).unwrap_or(true);

    // CRITICAL SAFETY: frankos-gateway cannot self-deploy.
    // The stop step kills this process before restart can complete — total system failure.
    // Build-only is safe. Deployment must be handed to Mac Frank.
    if service == "frankos-gateway" {
        return Ok(json!({
            "success": false,
            "error": "SAFETY BLOCK: frankos-gateway cannot self-deploy. Run cargo build to compile, then write READY_FOR_DEPLOYMENT to FRANK_TO_MAC.md. Mac Frank will stop, copy, and restart the service externally.",
            "binary_path": "/opt/frankos/runtime/frankos-gateway/target/release/frankos-gateway"
        }));
    }
    
    let service_dir = format!("/opt/frankos/runtime/{}", service);
    let build_mode = if release { "--release" } else { "" };
    let binary_path = if release {
        format!("{}/target/release/{}", service_dir, service)
    } else {
        format!("{}/target/debug/{}", service_dir, service)
    };
    
    // Step 1: Build
    let build_cmd = format!(
        "cd {} && RUSTUP_HOME=/root/.rustup CARGO_HOME=/root/.cargo /root/.cargo/bin/cargo build {}",
        service_dir, build_mode
    );
    
    let build_output = Command::new("sh")
        .arg("-c")
        .arg(&build_cmd)
        .output()
        .await?;
    
    if !build_output.status.success() {
        let stderr = String::from_utf8_lossy(&build_output.stderr);
        let last_30: Vec<&str> = stderr.lines().rev().take(30).collect();
        return Ok(json!({
            "success": false,
            "error": "Build failed",
            "output": last_30.into_iter().rev().collect::<Vec<_>>().join("\n")
        }));
    }
    
    let stdout = String::from_utf8_lossy(&build_output.stdout);
    let last_20_build: Vec<&str> = stdout.lines().rev().take(20).collect();
    
    // Step 2: Stop service
    let stop_output = Command::new("systemctl")
        .args(&["stop", service])
        .output()
        .await?;
    
    if !stop_output.status.success() {
        return Ok(json!({
            "success": false,
            "error": "Failed to stop service",
            "stderr": String::from_utf8_lossy(&stop_output.stderr)
        }));
    }
    
    // Step 3: Copy binary
    let copy_output = Command::new("cp")
        .arg(&binary_path)
        .arg(format!("/opt/frankos/bin/{}", service))
        .output()
        .await?;
    
    if !copy_output.status.success() {
        // Attempt restart before returning error
        let _ = Command::new("systemctl").args(&["start", service]).output().await;
        return Ok(json!({
            "success": false,
            "error": "Failed to copy binary",
            "stderr": String::from_utf8_lossy(&copy_output.stderr)
        }));
    }
    
    // Step 4: Start service
    let start_output = Command::new("systemctl")
        .args(&["start", service])
        .output()
        .await?;
    
    if !start_output.status.success() {
        return Ok(json!({
            "success": false,
            "error": "Failed to start service",
            "stderr": String::from_utf8_lossy(&start_output.stderr)
        }));
    }
    
    // Step 5: Wait 2 seconds for startup
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
    
    // Step 6: Health check
    let health_response = match reqwest::get("http://127.0.0.1:8080/health").await {
        Ok(resp) => match resp.text().await {
            Ok(text) => text,
            Err(_) => "Failed to read response".to_string(),
        },
        Err(_) => "No response".to_string(),
    };
    
    Ok(json!({
        "success": true,
        "build_lines": last_20_build.into_iter().rev().collect::<Vec<_>>(),
        "health": health_response,
        "binary": format!("/opt/frankos/bin/{}", service)
    }))
}

/// Tool 2: db_migration — apply SQL migration transactionally
async fn exec_db_migration(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let name = input["name"].as_str().ok_or_else(|| anyhow!("Missing name"))?;
    let sql = input["sql"].as_str().ok_or_else(|| anyhow!("Missing sql"))?;
    let rollback_sql = input.get("rollback_sql").and_then(|v| v.as_str());
    
    // Ensure migration tracking table exists
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS frank_schema_migrations (
            name TEXT PRIMARY KEY,
            applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            checksum TEXT
        )"
    ).execute(&ctx.db).await?;
    
    // Check if already applied
    let exists: Option<String> = sqlx::query_scalar(
        "SELECT name FROM frank_schema_migrations WHERE name = $1"
    ).bind(name).fetch_optional(&ctx.db).await?;
    
    if exists.is_some() {
        return Ok(json!({
            "success": true,
            "skipped": true,
            "name": name,
            "message": "Migration already applied"
        }));
    }
    
    // Begin transaction
    let mut tx = ctx.db.begin().await?;
    
    // Execute migration
    match sqlx::query(sql).execute(&mut *tx).await {
        Ok(_) => {
            // Record migration
            sqlx::query(
                "INSERT INTO frank_schema_migrations (name, applied_at) VALUES ($1, NOW())"
            ).bind(name).execute(&mut *tx).await?;
            
            // Commit
            tx.commit().await?;
            
            Ok(json!({
                "success": true,
                "name": name,
                "applied_at": chrono::Utc::now().to_rfc3339()
            }))
        }
        Err(e) => {
            // Rollback
            tx.rollback().await?;
            
            // Attempt rollback SQL if provided
            if let Some(rollback) = rollback_sql {
                let _ = sqlx::query(rollback).execute(&ctx.db).await;
            }
            
            Ok(json!({
                "success": false,
                "error": format!("Migration failed: {}", e),
                "name": name
            }))
        }
    }
}

/// Tool 3: agent_spawn — spawn agent with proper FK context
/// Tool 3: agent_spawn — spawn agent with persistent agent support
async fn exec_agent_spawn_fixed(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let name = input["name"].as_str().ok_or_else(|| anyhow!("Missing name"))?;
    let goal = input["goal"].as_str().ok_or_else(|| anyhow!("Missing goal"))?;
    let model = input.get("model").and_then(|v| v.as_str()).unwrap_or("claude-sonnet-4-5");
    let mut context = input.get("context").and_then(|v| v.as_str()).unwrap_or("").to_string();
    
    // Check if this matches a persistent agent
    let persistent: Option<(uuid::Uuid, String, String)> = sqlx::query_as(
        "SELECT id, system_prompt, memory_ns FROM frank_persistent_agents WHERE name = $1 AND status != 'archived'"
    ).bind(name).fetch_optional(&ctx.db).await?;
    
    let agent_id = uuid::Uuid::new_v4();
    
    if let Some((persistent_id, system_prompt, memory_ns)) = persistent {
        // This is a persistent agent — use its system prompt
        if context.is_empty() {
            context = system_prompt.clone();
        } else {
            context = format!("{}\n\n---\n\nAdditional context for this task:\n{}", system_prompt, context);
        }
        
        // Insert into frankos_agents (ephemeral spawn record)
        sqlx::query(
            "INSERT INTO frankos_agents (id, name, goal, model, parent_session_id, user_id, status, tools_allowed)
             VALUES ($1, $2, $3, $4, $5, $6, 'spawned', '[]')"
        )
        .bind(agent_id)
        .bind(name)
        .bind(goal)
        .bind(model)
        .bind(ctx.session_id)
        .bind(ctx.user_id)
        .execute(&ctx.db)
        .await?;
        
        // Log initial context to persistent conversation history (using persistent_id, not agent_id)
        sqlx::query(
            "INSERT INTO frank_agent_conversations (agent_id, role, content) VALUES ($1, 'system', $2)"
        ).bind(persistent_id).bind(&context).execute(&ctx.db).await?;
        
        // Log the goal as a user message
        sqlx::query(
            "INSERT INTO frank_agent_conversations (agent_id, role, content) VALUES ($1, 'user', $2)"
        ).bind(persistent_id).bind(goal).execute(&ctx.db).await?;
        
        Ok(json!({
            "success": true,
            "agent_id": agent_id,
            "persistent_agent_id": persistent_id,
            "parent_session_id": ctx.session_id,
            "name": name,
            "goal": goal,
            "memory_namespace": memory_ns
        }))
    } else {
        // Ephemeral agent — skip conversation insert to avoid FK errors
        
        // Insert into frankos_agents
        sqlx::query(
            "INSERT INTO frankos_agents (id, name, goal, model, parent_session_id, user_id, status, tools_allowed)
             VALUES ($1, $2, $3, $4, $5, $6, 'spawned', '[]')"
        )
        .bind(agent_id)
        .bind(name)
        .bind(goal)
        .bind(model)
        .bind(ctx.session_id)
        .bind(ctx.user_id)
        .execute(&ctx.db)
        .await?;
        
        Ok(json!({
            "success": true,
            "agent_id": agent_id,
            "parent_session_id": ctx.session_id,
            "name": name,
            "goal": goal
        }))
    }
}


/// Tool 4: memory_commit — batch write memory entries
async fn exec_memory_commit(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let entries = input["entries"].as_array().ok_or_else(|| anyhow!("Missing entries array"))?;
    
    if entries.len() > 10 {
        return Ok(json!({
            "success": false,
            "error": "Max 10 entries per batch"
        }));
    }
    
    let mut written = 0;
    let mut updated = 0;
    let mut skipped = 0;
    let mut ids = vec![];
    
    for entry in entries {
        let title = entry["title"].as_str().ok_or_else(|| anyhow!("Missing title"))?;
        let content = entry["content"].as_str().ok_or_else(|| anyhow!("Missing content"))?;
        let memory_type = entry.get("memory_type").and_then(|v| v.as_str()).unwrap_or("concept");
        let importance = entry.get("importance").and_then(|v| v.as_i64()).unwrap_or(5) as i32;
        let tags: Vec<String> = entry.get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        
        // Check if exists
        let existing: Option<(uuid::Uuid, i32)> = sqlx::query_as(
            "SELECT id, importance FROM frankos_memory 
             WHERE title = $1 AND namespace = 'chuck_frank' AND is_active = true"
        ).bind(title).fetch_optional(&ctx.db).await?;
        
        if let Some((existing_id, existing_importance)) = existing {
            if importance > existing_importance {
                // Update
                sqlx::query(
                    "UPDATE frankos_memory SET content = $1, importance = $2, updated_at = NOW() WHERE id = $3"
                ).bind(content).bind(importance).bind(existing_id).execute(&ctx.db).await?;
                updated += 1;
                ids.push(existing_id);
            } else {
                skipped += 1;
            }
        } else {
            // Insert new
            let new_id = crate::memory::store(
                &ctx.db,
                ctx.chat_bucket.as_str(),
                "chuck_frank",
                memory_type,
                title,
                content,
                importance,
                &tags,
                None,
                None,
                Some(ctx.session_id),
                "tool"
            ).await?;
            written += 1;
            ids.push(new_id);
        }
    }
    
    Ok(json!({
        "success": true,
        "written": written,
        "updated": updated,
        "skipped": skipped,
        "ids": ids
    }))
}

// ── Gap 10B: Escalation Mailbox Tools ─────────────────────────────────────────

/// Tool: mailbox_write — write message to agent mailbox
async fn exec_mailbox_write(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let to_agent_id = input.get("to_agent_id")
        .and_then(|v| v.as_str())
        .and_then(|s| uuid::Uuid::parse_str(s).ok());
    
    let message_type = input["message_type"].as_str()
        .ok_or_else(|| anyhow!("Missing message_type"))?;
    let subject = input["subject"].as_str()
        .ok_or_else(|| anyhow!("Missing subject"))?;
    let content = input["content"].as_str()
        .ok_or_else(|| anyhow!("Missing content"))?;
    
    let mailbox_id = uuid::Uuid::new_v4();
    
    sqlx::query(
        "INSERT INTO frank_agent_mailbox 
         (id, from_agent_id, to_agent_id, message_type, subject, content, status, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, 'unread', NOW())"
    )
    .bind(mailbox_id)
    .bind(ctx.user_id) // from_agent_id = current agent/user
    .bind(to_agent_id) // to_agent_id = null means SuperFrank
    .bind(message_type)
    .bind(subject)
    .bind(content)
    .execute(&ctx.db)
    .await?;
    
    Ok(json!({
        "success": true,
        "mailbox_id": mailbox_id,
        "to_agent_id": to_agent_id,
        "message_type": message_type
    }))
}

/// Tool: mailbox_read — read messages from mailbox
async fn exec_mailbox_read(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let to_agent_id = input.get("to_agent_id")
        .and_then(|v| v.as_str())
        .and_then(|s| uuid::Uuid::parse_str(s).ok());
    
    let status = input.get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unread");
    
    let limit = input.get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(10) as i32;
    
    // Query mailbox — if to_agent_id is None, check for IS NULL (SuperFrank)
    let messages: Vec<(uuid::Uuid, Option<uuid::Uuid>, String, String, String, chrono::DateTime<chrono::Utc>)> = if let Some(agent_id) = to_agent_id {
        sqlx::query_as(
            "SELECT id, from_agent_id, message_type, subject, content, created_at
             FROM frank_agent_mailbox
             WHERE to_agent_id = $1 AND status = $2
             ORDER BY created_at ASC
             LIMIT $3"
        )
        .bind(agent_id)
        .bind(status)
        .bind(limit)
        .fetch_all(&ctx.db)
        .await?
    } else {
        sqlx::query_as(
            "SELECT id, from_agent_id, message_type, subject, content, created_at
             FROM frank_agent_mailbox
             WHERE to_agent_id IS NULL AND status = $1
             ORDER BY created_at ASC
             LIMIT $2"
        )
        .bind(status)
        .bind(limit)
        .fetch_all(&ctx.db)
        .await?
    };
    
    let formatted: Vec<Value> = messages.iter().map(|(id, from_id, msg_type, subj, cont, created)| {
        json!({
            "id": id,
            "from_agent_id": from_id,
            "message_type": msg_type,
            "subject": subj,
            "content": cont,
            "created_at": created.to_rfc3339()
        })
    }).collect();
    
    Ok(json!({
        "success": true,
        "messages": formatted,
        "count": formatted.len()
    }))
}

/// Tool: mailbox_mark_read — mark messages as read
async fn exec_mailbox_mark_read(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let mailbox_ids = input["mailbox_ids"].as_array()
        .ok_or_else(|| anyhow!("Missing mailbox_ids array"))?;
    
    let mut uuids = vec![];
    for id in mailbox_ids {
        if let Some(id_str) = id.as_str() {
            if let Ok(uuid) = uuid::Uuid::parse_str(id_str) {
                uuids.push(uuid);
            }
        }
    }
    
    if uuids.is_empty() {
        return Ok(json!({
            "success": false,
            "error": "No valid UUIDs provided"
        }));
    }
    
    let result = sqlx::query(
        "UPDATE frank_agent_mailbox SET status = 'read' WHERE id = ANY($1)"
    )
    .bind(&uuids)
    .execute(&ctx.db)
    .await?;
    
    Ok(json!({
        "success": true,
        "updated": result.rows_affected()
    }))
}

/// Tool: notify_internal — persist an internal notification for SuperFrank workflows
async fn exec_notify_internal(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let title = input["title"].as_str().ok_or_else(|| anyhow!("Missing title"))?;
    let body = input["body"].as_str().ok_or_else(|| anyhow!("Missing body"))?;
    let level = input["level"].as_str().unwrap_or("info");
    let source = input["source"].as_str().unwrap_or("agent");
    let metadata = input.get("metadata").cloned();

    let target_user_id = match input.get("target_user_id").and_then(|v| v.as_str()) {
        Some(id) => uuid::Uuid::parse_str(id).map_err(|_| anyhow!("Invalid target_user_id UUID"))?,
        None => ctx.user_id,
    };

    let notification_id = sqlx::query_scalar::<_, uuid::Uuid>(
        "INSERT INTO frank_notifications (user_id, source, level, title, body, metadata, status)
         VALUES ($1, $2, $3, $4, $5, $6, 'queued')
         RETURNING id"
    )
    .bind(target_user_id)
    .bind(source)
    .bind(level)
    .bind(title)
    .bind(body)
    .bind(metadata)
    .fetch_one(&ctx.db)
    .await?;

    Ok(json!({
        "success": true,
        "notification_id": notification_id,
        "target_user_id": target_user_id,
        "level": level,
        "source": source
    }))
}

/// Tool: notification_inbox — read internal notifications for the current user
async fn exec_notification_inbox(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let status = input["status"].as_str().unwrap_or("queued");
    let source_filter = input.get("source").and_then(|v| v.as_str());
    let limit = input["limit"].as_i64().unwrap_or(20).clamp(1, 100) as i64;

    let rows: Vec<(
        uuid::Uuid,
        String,
        String,
        String,
        String,
        Option<Value>,
        String,
        Option<String>,
        chrono::DateTime<chrono::Utc>,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = if let Some(source) = source_filter {
        sqlx::query_as(
            "SELECT id, source, level, title, body, metadata, status, delivered_via, created_at, delivered_at, acknowledged_at
             FROM frank_notifications
             WHERE user_id = $1 AND status = $2 AND source = $3
             ORDER BY created_at DESC
             LIMIT $4"
        )
        .bind(ctx.user_id)
        .bind(status)
        .bind(source)
        .bind(limit)
        .fetch_all(&ctx.db)
        .await?
    } else {
        sqlx::query_as(
            "SELECT id, source, level, title, body, metadata, status, delivered_via, created_at, delivered_at, acknowledged_at
             FROM frank_notifications
             WHERE user_id = $1 AND status = $2
             ORDER BY created_at DESC
             LIMIT $3"
        )
        .bind(ctx.user_id)
        .bind(status)
        .bind(limit)
        .fetch_all(&ctx.db)
        .await?
    };

    let notifications: Vec<Value> = rows
        .into_iter()
        .map(|(id, source, level, title, body, metadata, status, delivered_via, created_at, delivered_at, acknowledged_at)| {
            json!({
                "id": id,
                "source": source,
                "level": level,
                "title": title,
                "body": body,
                "metadata": metadata,
                "status": status,
                "delivered_via": delivered_via,
                "created_at": created_at.to_rfc3339(),
                "delivered_at": delivered_at.map(|v| v.to_rfc3339()),
                "acknowledged_at": acknowledged_at.map(|v| v.to_rfc3339())
            })
        })
        .collect();

    Ok(json!({
        "success": true,
        "count": notifications.len(),
        "notifications": notifications
    }))
}

/// Tool: notification_ack — acknowledge notification IDs
async fn exec_notification_ack(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let ids = input["notification_ids"]
        .as_array()
        .ok_or_else(|| anyhow!("Missing notification_ids"))?;

    let mut parsed_ids = Vec::with_capacity(ids.len());
    for id in ids {
        let s = id.as_str().ok_or_else(|| anyhow!("notification_ids must be strings"))?;
        parsed_ids.push(uuid::Uuid::parse_str(s).map_err(|_| anyhow!("Invalid notification UUID: {}", s))?);
    }

    if parsed_ids.is_empty() {
        return Ok(json!({
            "success": false,
            "error": "No notification IDs provided"
        }));
    }

    let result = sqlx::query(
        "UPDATE frank_notifications
         SET status = 'acknowledged', acknowledged_at = NOW()
         WHERE user_id = $1 AND id = ANY($2::uuid[])"
    )
    .bind(ctx.user_id)
    .bind(&parsed_ids)
    .execute(&ctx.db)
    .await?;

    Ok(json!({
        "success": true,
        "updated": result.rows_affected()
    }))
}


// Include expanded capability tools
include!("tools_extended.rs");
