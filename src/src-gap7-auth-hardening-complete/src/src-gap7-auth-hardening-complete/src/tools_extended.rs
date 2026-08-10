

// ═══════════════════════════════════════════════════════════════════════════════
// NEW TOOLS — Expanded Capability Pack (2026-08-09)
// ═══════════════════════════════════════════════════════════════════════════════

// ── Email (Resend) ────────────────────────────────────────────────────────────

/// Tool: send_email — send an email via Resend
async fn exec_send_email(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let to = input["to"].as_str().ok_or_else(|| anyhow!("Missing to"))?;
    let subject = input["subject"].as_str().ok_or_else(|| anyhow!("Missing subject"))?;
    let body = input["body"].as_str().ok_or_else(|| anyhow!("Missing body"))?;
    let from = input.get("from").and_then(|v| v.as_str()).unwrap_or("frank@swarmlogic.cloud");

    let key = std::env::var("RESEND_API_KEY").map_err(|_| anyhow!("RESEND_API_KEY not set"))?;

    let is_html = body.trim_start().starts_with('<');
    let html_body = if is_html {
        body.to_string()
    } else {
        format!(
            "<div style='font-family:sans-serif;max-width:600px;margin:auto;padding:24px'>\
             <p style='color:#333;line-height:1.6'>{}</p>\
             <hr style='border:1px solid #eee;margin:24px 0'/>\
             <p style='color:#999;font-size:12px'>Frank — SwarmLogic</p></div>",
            body.replace('\n', "<br>")
        )
    };

    let payload = serde_json::json!({
        "from": format!("Frank <{}>", from),
        "to": [to],
        "subject": subject,
        "html": html_body,
    });

    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.resend.com/emails")
        .header("Authorization", format!("Bearer {}", key))
        .json(&payload)
        .send().await?;

    if resp.status().is_success() {
        let data: Value = resp.json().await.unwrap_or(serde_json::json!({}));
        Ok(serde_json::json!({"success": true, "id": data.get("id"), "to": to, "subject": subject}))
    } else {
        let err = resp.text().await?;
        Ok(serde_json::json!({"success": false, "error": err}))
    }
}

// ── GitHub ────────────────────────────────────────────────────────────────────

async fn github_request(method: &str, path: &str, body: Option<Value>) -> Result<Value> {
    let token = std::env::var("GITHUB_TOKEN").map_err(|_| anyhow!("GITHUB_TOKEN not set"))?;
    let url = if path.starts_with("https://") { path.to_string() } else { format!("https://api.github.com{}", path) };
    let client = reqwest::Client::new();
    let mut req = match method {
        "POST" => client.post(&url),
        "PATCH" => client.patch(&url),
        "PUT" => client.put(&url),
        "DELETE" => client.delete(&url),
        _ => client.get(&url),
    };
    req = req
        .header("Authorization", format!("Bearer {}", token))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "SuperFrank/3.0")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req.send().await?;
    let status = resp.status().as_u16();
    let data: Value = resp.json().await.unwrap_or(serde_json::json!({}));
    if status >= 400 {
        Ok(serde_json::json!({"success": false, "status": status, "error": data}))
    } else {
        Ok(serde_json::json!({"success": true, "status": status, "data": data}))
    }
}

/// Tool: github_list_repos — list repos for a user or org
async fn exec_github_list_repos(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let owner = input["owner"].as_str().ok_or_else(|| anyhow!("Missing owner"))?;
    let kind = input.get("kind").and_then(|v| v.as_str()).unwrap_or("users");
    github_request("GET", &format!("/{}/{}/repos?per_page=50&sort=updated", kind, owner), None).await
}

/// Tool: github_get_repo — get repo details
async fn exec_github_get_repo(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let owner = input["owner"].as_str().ok_or_else(|| anyhow!("Missing owner"))?;
    let repo = input["repo"].as_str().ok_or_else(|| anyhow!("Missing repo"))?;
    github_request("GET", &format!("/repos/{}/{}", owner, repo), None).await
}

/// Tool: github_list_issues — list issues for a repo
async fn exec_github_list_issues(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let owner = input["owner"].as_str().ok_or_else(|| anyhow!("Missing owner"))?;
    let repo = input["repo"].as_str().ok_or_else(|| anyhow!("Missing repo"))?;
    let state = input.get("state").and_then(|v| v.as_str()).unwrap_or("open");
    let labels = input.get("labels").and_then(|v| v.as_str()).unwrap_or("");
    github_request("GET", &format!("/repos/{}/{}/issues?state={}&labels={}&per_page=30", owner, repo, state, labels), None).await
}

/// Tool: github_create_issue — create an issue
async fn exec_github_create_issue(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let owner = input["owner"].as_str().ok_or_else(|| anyhow!("Missing owner"))?;
    let repo = input["repo"].as_str().ok_or_else(|| anyhow!("Missing repo"))?;
    let body = serde_json::json!({
        "title": input["title"].as_str().unwrap_or(""),
        "body": input.get("body").and_then(|v| v.as_str()).unwrap_or(""),
        "labels": input.get("labels").cloned().unwrap_or(serde_json::json!([])),
    });
    github_request("POST", &format!("/repos/{}/{}/issues", owner, repo), Some(body)).await
}

/// Tool: github_list_prs — list pull requests
async fn exec_github_list_prs(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let owner = input["owner"].as_str().ok_or_else(|| anyhow!("Missing owner"))?;
    let repo = input["repo"].as_str().ok_or_else(|| anyhow!("Missing repo"))?;
    let state = input.get("state").and_then(|v| v.as_str()).unwrap_or("open");
    github_request("GET", &format!("/repos/{}/{}/pulls?state={}&per_page=20", owner, repo, state), None).await
}

/// Tool: github_create_pr — create a pull request
async fn exec_github_create_pr(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let owner = input["owner"].as_str().ok_or_else(|| anyhow!("Missing owner"))?;
    let repo = input["repo"].as_str().ok_or_else(|| anyhow!("Missing repo"))?;
    let body = serde_json::json!({
        "title": input["title"].as_str().unwrap_or(""),
        "body": input.get("body").and_then(|v| v.as_str()).unwrap_or(""),
        "head": input["head"].as_str().unwrap_or(""),
        "base": input.get("base").and_then(|v| v.as_str()).unwrap_or("main"),
    });
    github_request("POST", &format!("/repos/{}/{}/pulls", owner, repo), Some(body)).await
}

/// Tool: github_get_file — get file contents from a repo
async fn exec_github_get_file(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let owner = input["owner"].as_str().ok_or_else(|| anyhow!("Missing owner"))?;
    let repo = input["repo"].as_str().ok_or_else(|| anyhow!("Missing repo"))?;
    let path = input["path"].as_str().ok_or_else(|| anyhow!("Missing path"))?;
    let branch = input.get("branch").and_then(|v| v.as_str()).unwrap_or("main");
    let result = github_request("GET", &format!("/repos/{}/{}/contents/{}?ref={}", owner, repo, path, branch), None).await?;
    // Decode base64 content if present
    if let Some(content_b64) = result.get("data").and_then(|d| d.get("content")).and_then(|c| c.as_str()) {
        let cleaned = content_b64.replace('\n', "");
        use std::io::Read;
        let decoded = match base64_decode(&cleaned) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
            Err(_) => content_b64.to_string(),
        };
        return Ok(serde_json::json!({"success": true, "content": decoded, "path": path}));
    }
    Ok(result)
}

fn base64_decode(s: &str) -> Result<Vec<u8>> {
    use std::collections::HashMap;
    let table: HashMap<char, u8> = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
        .chars().enumerate().map(|(i, c)| (c, i as u8)).collect();
    let mut out = vec![];
    let chars: Vec<char> = s.chars().filter(|c| *c != '=').collect();
    let mut i = 0;
    while i + 3 < chars.len() {
        let a = *table.get(&chars[i]).ok_or_else(|| anyhow!("bad base64"))? as u32;
        let b = *table.get(&chars[i+1]).ok_or_else(|| anyhow!("bad base64"))? as u32;
        let c = *table.get(&chars[i+2]).ok_or_else(|| anyhow!("bad base64"))? as u32;
        let d = *table.get(&chars[i+3]).ok_or_else(|| anyhow!("bad base64"))? as u32;
        out.push(((a << 2) | (b >> 4)) as u8);
        out.push(((b << 4) | (c >> 2)) as u8);
        out.push(((c << 6) | d) as u8);
        i += 4;
    }
    Ok(out)
}

/// Tool: github_search_code — search code across GitHub
async fn exec_github_search_code(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let query = input["query"].as_str().ok_or_else(|| anyhow!("Missing query"))?;
    github_request("GET", &format!("/search/code?q={}&per_page=10", urlencoding_simple(query)), None).await
}

fn urlencoding_simple(s: &str) -> String {
    s.chars().map(|c| match c {
        'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | ':' | '/' => c.to_string(),
        ' ' => "+".to_string(),
        _ => format!("%{:02X}", c as u32),
    }).collect()
}

// ── Cloudflare ────────────────────────────────────────────────────────────────

async fn cf_request(method: &str, path: &str, body: Option<Value>) -> Result<Value> {
    let token = std::env::var("CF_DNS_TOKEN").or_else(|_| std::env::var("CF_API_TOKEN"))
        .map_err(|_| anyhow!("CF_DNS_TOKEN not set"))?;
    let url = format!("https://api.cloudflare.com/client/v4{}", path);
    let client = reqwest::Client::new();
    let mut req = match method {
        "POST" => client.post(&url),
        "PATCH" => client.patch(&url),
        "PUT" => client.put(&url),
        "DELETE" => client.delete(&url),
        _ => client.get(&url),
    };
    req = req.header("Authorization", format!("Bearer {}", token)).header("Content-Type", "application/json");
    if let Some(b) = body { req = req.json(&b); }
    let resp = req.send().await?;
    let data: Value = resp.json().await.unwrap_or(serde_json::json!({}));
    Ok(data)
}

/// Tool: cf_list_dns — list DNS records for a zone
async fn exec_cf_list_dns(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let zone_id = input.get("zone_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| std::env::var("CF_ZONE_ID").unwrap_or_default());
    let record_type = input.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let filter = if record_type.is_empty() { String::new() } else { format!("&type={}", record_type) };
    cf_request("GET", &format!("/zones/{}/dns_records?per_page=100{}", zone_id, filter), None).await
}

/// Tool: cf_create_dns — create a DNS record
async fn exec_cf_create_dns(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let zone_id = input.get("zone_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| std::env::var("CF_ZONE_ID").unwrap_or_default());
    let body = serde_json::json!({
        "type": input["type"].as_str().unwrap_or("A"),
        "name": input["name"].as_str().unwrap_or(""),
        "content": input["content"].as_str().unwrap_or(""),
        "ttl": input.get("ttl").and_then(|v| v.as_i64()).unwrap_or(1),
        "proxied": input.get("proxied").and_then(|v| v.as_bool()).unwrap_or(false),
    });
    cf_request("POST", &format!("/zones/{}/dns_records", zone_id), Some(body)).await
}

/// Tool: cf_delete_dns — delete a DNS record
async fn exec_cf_delete_dns(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let zone_id = input.get("zone_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| std::env::var("CF_ZONE_ID").unwrap_or_default());
    let record_id = input["record_id"].as_str().ok_or_else(|| anyhow!("Missing record_id"))?;
    cf_request("DELETE", &format!("/zones/{}/dns_records/{}", zone_id, record_id), None).await
}

/// Tool: cf_purge_cache — purge Cloudflare cache
async fn exec_cf_purge_cache(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let zone_id = input.get("zone_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| std::env::var("CF_ZONE_ID").unwrap_or_default());
    let body = if let Some(urls) = input.get("urls") {
        serde_json::json!({"files": urls})
    } else {
        serde_json::json!({"purge_everything": true})
    };
    cf_request("POST", &format!("/zones/{}/purge_cache", zone_id), Some(body)).await
}

// ── Hetzner ───────────────────────────────────────────────────────────────────

async fn hetzner_request(method: &str, path: &str, body: Option<Value>) -> Result<Value> {
    let token = std::env::var("HETZNER_API_TOKEN").map_err(|_| anyhow!("HETZNER_API_TOKEN not set"))?;
    let url = format!("https://api.hetzner.cloud/v1{}", path);
    let client = reqwest::Client::new();
    let mut req = match method {
        "POST" => client.post(&url),
        "DELETE" => client.delete(&url),
        "PUT" => client.put(&url),
        _ => client.get(&url),
    };
    req = req.header("Authorization", format!("Bearer {}", token));
    if let Some(b) = body { req = req.json(&b); }
    let resp = req.send().await?;
    let data: Value = resp.json().await.unwrap_or(serde_json::json!({}));
    Ok(data)
}

/// Tool: hetzner_list_servers — list all Hetzner servers
async fn exec_hetzner_list_servers(_input: &Value, _ctx: &ToolContext) -> Result<Value> {
    hetzner_request("GET", "/servers", None).await
}

/// Tool: hetzner_server_action — perform an action on a server (reboot, reset, poweron, poweroff)
async fn exec_hetzner_server_action(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let server_id = input["server_id"].as_i64().ok_or_else(|| anyhow!("Missing server_id"))?;
    let action = input["action"].as_str().ok_or_else(|| anyhow!("Missing action"))?;
    hetzner_request("POST", &format!("/servers/{}/actions/{}", server_id, action), None).await
}

/// Tool: hetzner_get_server — get details for one server
async fn exec_hetzner_get_server(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let server_id = input["server_id"].as_i64().ok_or_else(|| anyhow!("Missing server_id"))?;
    hetzner_request("GET", &format!("/servers/{}", server_id), None).await
}

// ── Weather ───────────────────────────────────────────────────────────────────

/// Tool: get_weather — get current weather for a location
async fn exec_get_weather(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let location = input["location"].as_str().ok_or_else(|| anyhow!("Missing location"))?;
    let format = input.get("format").and_then(|v| v.as_str()).unwrap_or("j1");
    let url = format!("https://wttr.in/{}?format={}", urlencoding_simple(location), format);
    let client = reqwest::Client::new();
    let resp = client.get(&url).header("User-Agent", "SuperFrank/3.0").send().await?;
    let text = resp.text().await?;
    if format == "j1" {
        let json: Value = serde_json::from_str(&text).unwrap_or(serde_json::json!({"raw": text}));
        Ok(serde_json::json!({"success": true, "location": location, "weather": json}))
    } else {
        Ok(serde_json::json!({"success": true, "location": location, "weather": text}))
    }
}

// ── Notion ────────────────────────────────────────────────────────────────────

async fn notion_request(method: &str, path: &str, body: Option<Value>) -> Result<Value> {
    let token = std::env::var("NOTION_TOKEN").map_err(|_| anyhow!("NOTION_TOKEN not set — add it to .env"))?;
    let url = format!("https://api.notion.com/v1{}", path);
    let client = reqwest::Client::new();
    let mut req = match method {
        "POST" => client.post(&url),
        "PATCH" => client.patch(&url),
        _ => client.get(&url),
    };
    req = req
        .header("Authorization", format!("Bearer {}", token))
        .header("Notion-Version", "2022-06-28")
        .header("Content-Type", "application/json");
    if let Some(b) = body { req = req.json(&b); }
    let resp = req.send().await?;
    let data: Value = resp.json().await.unwrap_or(serde_json::json!({}));
    Ok(data)
}

/// Tool: notion_search — search Notion workspace
async fn exec_notion_search(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let query = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
    notion_request("POST", "/search", Some(serde_json::json!({"query": query, "page_size": 20}))).await
}

/// Tool: notion_get_page — get a Notion page by ID
async fn exec_notion_get_page(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let page_id = input["page_id"].as_str().ok_or_else(|| anyhow!("Missing page_id"))?;
    notion_request("GET", &format!("/pages/{}", page_id), None).await
}

/// Tool: notion_create_page — create a new Notion page
async fn exec_notion_create_page(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let parent_id = input["parent_id"].as_str().ok_or_else(|| anyhow!("Missing parent_id"))?;
    let title = input["title"].as_str().ok_or_else(|| anyhow!("Missing title"))?;
    let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let body = serde_json::json!({
        "parent": {"page_id": parent_id},
        "properties": {
            "title": {"title": [{"text": {"content": title}}]}
        },
        "children": [{
            "object": "block",
            "type": "paragraph",
            "paragraph": {"rich_text": [{"text": {"content": content}}]}
        }]
    });
    notion_request("POST", "/pages", Some(body)).await
}

/// Tool: notion_append_block — append content blocks to a Notion page
async fn exec_notion_append_block(input: &Value, _ctx: &ToolContext) -> Result<Value> {
    let block_id = input["block_id"].as_str().ok_or_else(|| anyhow!("Missing block_id"))?;
    let content = input["content"].as_str().ok_or_else(|| anyhow!("Missing content"))?;
    let body = serde_json::json!({
        "children": [{
            "object": "block",
            "type": "paragraph",
            "paragraph": {"rich_text": [{"text": {"content": content}}]}
        }]
    });
    notion_request("POST", &format!("/blocks/{}/children", block_id), Some(body)).await
}

// ── OpenAI Image Generation ───────────────────────────────────────────────────

/// Tool: generate_image_openai — generate an image using DALL-E 3
async fn exec_generate_image_openai(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let prompt = input["prompt"].as_str().ok_or_else(|| anyhow!("Missing prompt"))?;
    let size = input.get("size").and_then(|v| v.as_str()).unwrap_or("1024x1024");
    let quality = input.get("quality").and_then(|v| v.as_str()).unwrap_or("standard");
    let model = input.get("model").and_then(|v| v.as_str()).unwrap_or("dall-e-3");

    let api_key = ctx.openai_api_key.as_deref().ok_or_else(|| anyhow!("OPENAI_API_KEY not set"))?;
    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.openai.com/v1/images/generations")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "model": model,
            "prompt": prompt,
            "n": 1,
            "size": size,
            "quality": quality,
        }))
        .send().await?;

    if resp.status().is_success() {
        let data: Value = resp.json().await?;
        let url = data["data"][0]["url"].as_str().unwrap_or("").to_string();
        let revised = data["data"][0]["revised_prompt"].as_str().unwrap_or("").to_string();
        Ok(serde_json::json!({"success": true, "url": url, "revised_prompt": revised, "model": model}))
    } else {
        let err = resp.text().await?;
        Ok(serde_json::json!({"success": false, "error": err}))
    }
}

// ── Diagram Maker (SVG) ───────────────────────────────────────────────────────

/// Tool: make_diagram — generate a simple SVG diagram from a description
/// Uses Gemini/Anthropic to write SVG code, saves to workspace, returns path
async fn exec_make_diagram(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let description = input["description"].as_str().ok_or_else(|| anyhow!("Missing description"))?;
    let filename = input.get("filename").and_then(|v| v.as_str()).unwrap_or("diagram.svg");
    let out_path = format!("/opt/frankos/workspace/diagrams/{}", filename);

    // Ask LLM to generate SVG
    let api_key = ctx.openai_api_key.as_deref().ok_or_else(|| anyhow!("OPENAI_API_KEY not set"))?;
    let client = reqwest::Client::new();
    let prompt = format!(
        "Generate a clean, well-structured SVG diagram for: {}\n\
         Return ONLY the raw SVG code starting with <svg, no markdown, no explanation.",
        description
    );
    let resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": 2000,
        }))
        .send().await?;

    let data: Value = resp.json().await?;
    let svg = data["choices"][0]["message"]["content"].as_str().unwrap_or("").trim().to_string();

    if svg.is_empty() || !svg.contains("<svg") {
        return Ok(serde_json::json!({"success": false, "error": "LLM did not return valid SVG"}));
    }

    // Ensure diagrams dir exists
    let _ = tokio::fs::create_dir_all("/opt/frankos/workspace/diagrams").await;
    tokio::fs::write(&out_path, &svg).await?;

    Ok(serde_json::json!({"success": true, "path": out_path, "bytes": svg.len()}))
}

// ── Summarize ─────────────────────────────────────────────────────────────────

/// Tool: summarize — summarize text or a URL
async fn exec_summarize(input: &Value, ctx: &ToolContext) -> Result<Value> {
    let api_key = ctx.openai_api_key.as_deref().ok_or_else(|| anyhow!("OPENAI_API_KEY not set"))?;

    let text = if let Some(url) = input.get("url").and_then(|v| v.as_str()) {
        // Fetch URL first
        let client = reqwest::Client::new();
        client.get(url).header("User-Agent", "SuperFrank/3.0").send().await?.text().await?
    } else {
        input["text"].as_str().ok_or_else(|| anyhow!("Missing text or url"))?.to_string()
    };

    let style = input.get("style").and_then(|v| v.as_str()).unwrap_or("concise");
    let prompt = format!(
        "Summarize the following in a {} style:\n\n{}",
        style, &text[..text.len().min(8000)]
    );

    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .json(&serde_json::json!({
            "model": "gpt-4o-mini",
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": 500,
        }))
        .send().await?;

    let data: Value = resp.json().await?;
    let summary = data["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string();
    Ok(serde_json::json!({"success": true, "summary": summary, "style": style}))
}
