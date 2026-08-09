//! LLM client — Anthropic + OpenAI with streaming and tool_use support

use anyhow::{anyhow, Result};
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::info;

use crate::agents::{AgentResponse, ToolCall};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub enum LlmProvider {
    Anthropic,
    OpenAI,
}

impl LlmProvider {
    pub fn from_str(s: &str) -> Self {
        match s {
            "openai" | "gpt" => LlmProvider::OpenAI,
            _ => LlmProvider::Anthropic,
        }
    }
}

pub struct LlmClient {
    http: Client,
    pub anthropic_key: Option<String>,
    pub openai_key: Option<String>,
}

impl LlmClient {
    pub fn new(anthropic_key: Option<String>, openai_key: Option<String>) -> Self {
        // Force HTTP/1.1 — Anthropic SSE streaming breaks on HTTP/2 with reqwest
        let http = reqwest::ClientBuilder::new()
            .http1_only()
            .build()
            .expect("reqwest client");
        Self {
            http,
            anthropic_key,
            openai_key,
        }
    }

    /// Non-streaming completion — returns full response text
    pub async fn complete(
        &self,
        provider: &LlmProvider,
        model: &str,
        system: &str,
        messages: &[ChatMessage],
        max_tokens: u32,
    ) -> Result<String> {
        match provider {
            LlmProvider::Anthropic => self.anthropic_complete(model, system, messages, max_tokens, &[]).await,
            LlmProvider::OpenAI => self.openai_complete(model, system, messages, max_tokens).await,
        }
    }

    /// Tool-use completion — returns either a text response or tool calls
    pub async fn complete_with_tools(
        &self,
        model: &str,
        system: &str,
        messages: &[ChatMessage],
        max_tokens: u32,
        tools: &[Value],
    ) -> Result<AgentResponse> {
        self.anthropic_complete_with_tools(model, system, messages, max_tokens, tools).await
    }

    /// Streaming completion with tool support — sends chunks + tool events via mpsc
    pub async fn stream_with_tools(
        &self,
        provider: &LlmProvider,
        model: &str,
        system: &str,
        messages: Vec<ChatMessage>,
        max_tokens: u32,
        tools: &[Value],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<String> {
        match provider {
            LlmProvider::Anthropic => self.anthropic_stream_with_tools(model, system, messages, max_tokens, tools, tx).await,
            LlmProvider::OpenAI => {
                // OpenAI path — tool use not yet implemented, fall back to plain stream
                let (plain_tx, mut plain_rx) = mpsc::channel::<String>(256);
                let text_tx = tx.clone();
                tokio::spawn(async move {
                    while let Some(chunk) = plain_rx.recv().await {
                        let _ = text_tx.send(StreamEvent::Delta(chunk)).await;
                    }
                });
                self.openai_stream(model, system, messages, max_tokens, {
                    let (s, _) = mpsc::channel(1); s
                }).await?;
                Ok(String::new())
            }
        }
    }
    /// Streaming with tool_use support — returns (text, tool_calls)
    /// Tool calls are returned so the caller can execute them and loop back.
    pub async fn stream_with_tools_and_calls(
        &self,
        provider: &LlmProvider,
        model: &str,
        system: &str,
        messages: Vec<ChatMessage>,
        max_tokens: u32,
        tools: &[Value],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<(String, Vec<ToolCallRequest>)> {
        match provider {
            LlmProvider::Anthropic => {
                self.anthropic_stream_with_tools_and_calls(model, system, messages, max_tokens, tools, tx).await
            }
            _ => {
                // Non-Anthropic: fall back, no tool calls
                let text = self.stream_with_tools(provider, model, system, messages, max_tokens, tools, tx).await?;
                Ok((text, vec![]))
            }
        }
    }

    async fn anthropic_stream_with_tools_and_calls(
        &self,
        model: &str,
        system: &str,
        messages: Vec<ChatMessage>,
        max_tokens: u32,
        tools: &[Value],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<(String, Vec<ToolCallRequest>)> {
        let key = self.anthropic_key.as_ref().ok_or_else(|| anyhow!("Anthropic API key not set"))?;

        let msgs = build_anthropic_messages(&messages);
        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": msgs,
            "stream": true,
        });

        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!({ "type": "auto" });
        }
        let response = self.http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send().await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("Anthropic API error {}: {}", status, body));
        }
        let mut stream = response.bytes_stream();

        let mut buf = String::new();
        let mut full_text = String::new();
        let mut tool_calls: Vec<ToolCallRequest> = vec![];

        // Current tool being accumulated
        let mut cur_id    = String::new();
        let mut cur_name  = String::new();
        let mut cur_input = String::new();
        let mut in_tool   = false;
        while let Some(chunk) = stream.next().await {            let chunk = chunk?;
            buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].to_string();
                buf = buf[pos+1..].to_string();

                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" { break; }
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        match v["type"].as_str() {
                            Some("content_block_start") => {
                                let btype = v["content_block"]["type"].as_str().unwrap_or("");
                                if btype == "tool_use" {
                                    in_tool   = true;
                                    cur_id    = v["content_block"]["id"].as_str().unwrap_or("").to_string();
                                    cur_name  = v["content_block"]["name"].as_str().unwrap_or("").to_string();
                                    cur_input = String::new();
                                    let _ = tx.send(StreamEvent::ToolStart {
                                        id: cur_id.clone(),
                                        name: cur_name.clone(),
                                    }).await;
                                }
                            }
                            Some("content_block_delta") => {
                                let dtype = v["delta"]["type"].as_str().unwrap_or("");
                                if dtype == "text_delta" {
                                    if let Some(text) = v["delta"]["text"].as_str() {
                                        full_text.push_str(text);
                                        let encoded = serde_json::to_string(text).unwrap_or_default();
                                        let _ = tx.send(StreamEvent::Delta(encoded)).await;
                                    }
                                } else if dtype == "input_json_delta" {
                                    if let Some(partial) = v["delta"]["partial_json"].as_str() {
                                        cur_input.push_str(partial);
                                    }
                                }
                            }
                            Some("content_block_stop") => {
                                if in_tool {
                                    let input_val: Value = serde_json::from_str(&cur_input)
                                        .unwrap_or(json!({}));
                                    // Notify UI of tool input
                                    let _ = tx.send(StreamEvent::ToolInput {
                                        id: cur_id.clone(),
                                        name: cur_name.clone(),
                                        input: input_val.clone(),
                                    }).await;
                                    tool_calls.push(ToolCallRequest {
                                        id:    cur_id.clone(),
                                        name:  cur_name.clone(),
                                        input: input_val,
                                    });
                                    in_tool = false;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        Ok((full_text, tool_calls))
    }
    /// Old streaming call kept for backward compat
    pub async fn stream(
        &self,
        provider: &LlmProvider,
        model: &str,
        system: &str,
        messages: Vec<ChatMessage>,
        max_tokens: u32,
        tx: mpsc::Sender<String>,
    ) -> Result<()> {
        match provider {
            LlmProvider::Anthropic => self.anthropic_stream_plain(model, system, messages, max_tokens, tx).await,
            LlmProvider::OpenAI => self.openai_stream(model, system, messages, max_tokens, tx).await,
        }
    }

    // ── Anthropic ─────────────────────────────────────────────────────────────

    async fn anthropic_complete(
        &self,
        model: &str,
        system: &str,
        messages: &[ChatMessage],
        max_tokens: u32,
        tools: &[Value],
    ) -> Result<String> {
        let key = self.anthropic_key.as_ref().ok_or_else(|| anyhow!("Anthropic API key not set"))?;

        let msgs = build_anthropic_messages(messages);
        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": msgs,
        });

        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }

        let resp = self.http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send().await?
            .json::<Value>().await?;

        // Find first text block
        if let Some(content_arr) = resp["content"].as_array() {
            for block in content_arr {
                if block["type"] == "text" {
                    if let Some(text) = block["text"].as_str() {
                        return Ok(text.to_string());
                    }
                }
            }
            // Empty content array with end_turn is valid (model finished after tools)
            if content_arr.is_empty() {
                let stop_reason = resp["stop_reason"].as_str().unwrap_or("");
                if stop_reason == "end_turn" {
                    return Ok(String::new());
                }
            }
        }

        Err(anyhow!("Unexpected Anthropic response: {}", resp))
    }

    async fn anthropic_complete_with_tools(
        &self,
        model: &str,
        system: &str,
        messages: &[ChatMessage],
        max_tokens: u32,
        tools: &[Value],
    ) -> Result<AgentResponse> {
        let key = self.anthropic_key.as_ref().ok_or_else(|| anyhow!("Anthropic API key not set"))?;

        let msgs = build_anthropic_messages(messages);
        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": msgs,
        });

        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!({ "type": "auto" });
        }

        let resp = self.http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send().await?
            .json::<Value>().await?;

        let stop_reason = resp["stop_reason"].as_str().unwrap_or("");

        if stop_reason == "tool_use" {
            // Parse tool_use blocks
            let mut tool_calls = Vec::new();
            if let Some(content_arr) = resp["content"].as_array() {
                for block in content_arr {
                    if block["type"] == "tool_use" {
                        tool_calls.push(ToolCall {
                            id: block["id"].as_str().unwrap_or("").to_string(),
                            name: block["name"].as_str().unwrap_or("").to_string(),
                            input: block["input"].clone(),
                        });
                    }
                }
            }
            Ok(AgentResponse::ToolUse(tool_calls))
        } else {
            // Text response
            if let Some(content_arr) = resp["content"].as_array() {
                for block in content_arr {
                    if block["type"] == "text" {
                        if let Some(text) = block["text"].as_str() {
                            return Ok(AgentResponse::Text(text.to_string()));
                        }
                    }
                }
                // Empty content with end_turn = model finished cleanly (e.g. after tool sequence)
                if content_arr.is_empty() && stop_reason == "end_turn" {
                    return Ok(AgentResponse::Text(String::new()));
                }
            }
            Err(anyhow!("No text or tool_use in response: {}", resp))
        }
    }

    /// Streaming with tool_use support — accumulates full response and returns it
    async fn anthropic_stream_with_tools(
        &self,
        model: &str,
        system: &str,
        messages: Vec<ChatMessage>,
        max_tokens: u32,
        tools: &[Value],
        tx: mpsc::Sender<StreamEvent>,
    ) -> Result<String> {
        let key = self.anthropic_key.as_ref().ok_or_else(|| anyhow!("Anthropic API key not set"))?;

        let msgs = build_anthropic_messages(&messages);
        let mut body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": msgs,
            "stream": true,
        });

        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["tool_choice"] = json!({ "type": "auto" });
        }

        let mut stream = self.http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send().await?
            .bytes_stream();

        let mut buf = String::new();
        let mut full_response = String::new();
        let mut current_tool_id = String::new();
        let mut current_tool_name = String::new();
        let mut current_tool_input = String::new();
        let mut in_tool_use = false;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].to_string();
                buf = buf[pos+1..].to_string();

                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" { break; }
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        match v["type"].as_str() {
                            Some("content_block_start") => {
                                let block_type = v["content_block"]["type"].as_str().unwrap_or("");
                                if block_type == "tool_use" {
                                    in_tool_use = true;
                                    current_tool_id = v["content_block"]["id"].as_str().unwrap_or("").to_string();
                                    current_tool_name = v["content_block"]["name"].as_str().unwrap_or("").to_string();
                                    current_tool_input = String::new();
                                    // Notify UI a tool is being called
                                    let _ = tx.send(StreamEvent::ToolStart {
                                        id: current_tool_id.clone(),
                                        name: current_tool_name.clone(),
                                    }).await;
                                }
                            }
                            Some("content_block_delta") => {
                                let delta_type = v["delta"]["type"].as_str().unwrap_or("");
                                if delta_type == "text_delta" {
                                    if let Some(text) = v["delta"]["text"].as_str() {
                                        full_response.push_str(text);
                                        let _ = tx.send(StreamEvent::Delta(text.to_string())).await;
                                    }
                                } else if delta_type == "input_json_delta" {
                                    if let Some(partial) = v["delta"]["partial_json"].as_str() {
                                        current_tool_input.push_str(partial);
                                    }
                                }
                            }
                            Some("content_block_stop") => {
                                if in_tool_use {
                                    // Parse and emit tool input
                                    let input: Value = serde_json::from_str(&current_tool_input)
                                        .unwrap_or(json!({}));
                                    let _ = tx.send(StreamEvent::ToolInput {
                                        id: current_tool_id.clone(),
                                        name: current_tool_name.clone(),
                                        input: input.clone(),
                                    }).await;
                                    in_tool_use = false;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        Ok(full_response)
    }

    /// Plain streaming (no tool support) — kept for backward compat
    async fn anthropic_stream_plain(
        &self,
        model: &str,
        system: &str,
        messages: Vec<ChatMessage>,
        max_tokens: u32,
        tx: mpsc::Sender<String>,
    ) -> Result<()> {
        let key = self.anthropic_key.as_ref().ok_or_else(|| anyhow!("Anthropic API key not set"))?;
        let msgs = build_anthropic_messages(&messages);

        let body = json!({
            "model": model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": msgs,
            "stream": true,
        });

        let mut stream = self.http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send().await?
            .bytes_stream();

        let mut buf = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].to_string();
                buf = buf[pos+1..].to_string();
                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" { break; }
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        if v["type"] == "content_block_delta" {
                            if let Some(text) = v["delta"]["text"].as_str() {
                                let _ = tx.send(text.to_string()).await;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    // ── OpenAI ────────────────────────────────────────────────────────────────

    async fn openai_complete(
        &self,
        model: &str,
        system: &str,
        messages: &[ChatMessage],
        max_tokens: u32,
    ) -> Result<String> {
        let key = self.openai_key.as_ref().ok_or_else(|| anyhow!("OpenAI API key not set"))?;

        let mut msgs = vec![json!({ "role": "system", "content": system })];
        for m in messages { msgs.push(json!({ "role": m.role, "content": m.content })); }

        let body = json!({ "model": model, "max_tokens": max_tokens, "messages": msgs });
        let resp = self.http
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", key))
            .json(&body).send().await?.json::<Value>().await?;

        resp["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| anyhow!("Unexpected OpenAI response: {}", resp))
            .map(|s| s.to_string())
    }

    async fn openai_stream(
        &self,
        model: &str,
        system: &str,
        messages: Vec<ChatMessage>,
        max_tokens: u32,
        tx: mpsc::Sender<String>,
    ) -> Result<()> {
        let key = self.openai_key.as_ref().ok_or_else(|| anyhow!("OpenAI API key not set"))?;

        let mut msgs = vec![json!({ "role": "system", "content": system })];
        for m in &messages { msgs.push(json!({ "role": m.role, "content": m.content })); }

        let body = json!({ "model": model, "max_tokens": max_tokens, "messages": msgs, "stream": true });
        let mut stream = self.http
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", key))
            .json(&body).send().await?.bytes_stream();

        let mut buf = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find('\n') {
                let line = buf[..pos].to_string();
                buf = buf[pos+1..].to_string();
                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" { break; }
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        if let Some(text) = v["choices"][0]["delta"]["content"].as_str() {
                            let _ = tx.send(text.to_string()).await;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

// ── Helper ────────────────────────────────────────────────────────────────────

fn build_anthropic_messages(messages: &[ChatMessage]) -> Vec<Value> {
    // First pass: build raw messages
    let raw: Vec<Value> = messages.iter().map(|m| {
        if m.role == "user" {
            if let Ok(arr) = serde_json::from_str::<Vec<Value>>(&m.content) {
                if arr.first().map(|v| v["type"] == "tool_result").unwrap_or(false) {
                    return json!({ "role": "user", "content": arr });
                }
            }
        }
        if m.role == "assistant" {
            if let Ok(arr) = serde_json::from_str::<Vec<Value>>(&m.content) {
                if arr.iter().any(|v| v["type"] == "tool_use") {
                    return json!({ "role": "assistant", "content": arr });
                }
            }
        }
        // Skip empty assistant messages — they confuse Anthropic
        if m.role == "assistant" && m.content.trim().is_empty() {
            return json!(null);
        }
        json!({ "role": m.role, "content": m.content })
    }).collect();

    // Second pass: validate tool_use/tool_result pairing and strip orphans
    let mut valid: Vec<Value> = Vec::new();
    let mut pending_tool_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for msg in raw {
        if msg.is_null() { continue; }
        let role = msg["role"].as_str().unwrap_or("");
        if role == "assistant" {
            // Collect tool_use ids from this assistant message
            if let Some(arr) = msg["content"].as_array() {
                pending_tool_ids.clear();
                for block in arr {
                    if block["type"] == "tool_use" {
                        if let Some(id) = block["id"].as_str() {
                            pending_tool_ids.insert(id.to_string());
                        }
                    }
                }
            }
            valid.push(msg);
        } else if role == "user" {
            // Check if this is a tool_result message
            if let Some(arr) = msg["content"].as_array() {
                let is_tool_result = arr.first().map(|v| v["type"] == "tool_result").unwrap_or(false);
                if is_tool_result {
                    // Validate all tool_result ids exist in pending_tool_ids
                    let all_valid = arr.iter().all(|block| {
                        block["tool_use_id"].as_str()
                            .map(|id| pending_tool_ids.contains(id))
                            .unwrap_or(false)
                    });
                    if !all_valid {
                        // Orphaned tool_result — skip it and the preceding assistant message
                        valid.pop(); // remove the orphaned assistant message
                        pending_tool_ids.clear();
                        continue;
                    }
                    pending_tool_ids.clear();
                }
            }
            valid.push(msg);
        } else {
            valid.push(msg);
        }
    }

    // Ensure we don't end on an assistant tool_use with no following tool_result
    while let Some(last) = valid.last() {
        if last["role"] == "assistant" {
            if let Some(arr) = last["content"].as_array() {
                if arr.iter().any(|v| v["type"] == "tool_use") {
                    valid.pop();
                    continue;
                }
            }
        }
        break;
    }

    valid
}

// ── Stream event types ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
/// A tool call requested by the LLM during streaming
pub struct ToolCallRequest {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

pub enum StreamEvent {
    Delta(String),
    Iteration { num: u32 },
    ToolStart { id: String, name: String },
    ToolInput  { id: String, name: String, input: Value },
    ToolResult { id: String, name: String, success: bool, output: Value, duration_ms: u64 },
    Notification { title: String, body: String },
    Done,
}

impl LlmClient {
    /// Simple one-shot completion — no tools, no streaming. Used by classifier.
    pub async fn complete_simple(
        &self,
        model: &str,
        prompt: &str,
        max_tokens: u32,
    ) -> anyhow::Result<String> {
        self.anthropic_complete(
            model,
            "You are a concise classifier.",
            &[ChatMessage { role: "user".to_string(), content: prompt.to_string() }],
            max_tokens,
            &[],
        ).await
    }
}
