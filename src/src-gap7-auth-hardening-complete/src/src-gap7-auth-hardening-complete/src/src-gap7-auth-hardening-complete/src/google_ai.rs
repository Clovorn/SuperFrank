//! Google AI tools — Gemini, Imagen 3, Veo 2
//! Exposes: generate_image, generate_video, analyze_image, gemini_chat, gemini_research

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tracing::info;

pub struct GoogleAiClient {
    http: reqwest::Client,
    pub api_key: Option<String>,
    pub project: Option<String>,
}

impl GoogleAiClient {
    pub fn new(api_key: Option<String>, project: Option<String>) -> Self {
        Self { http: reqwest::Client::new(), api_key, project }
    }

    fn key(&self) -> Result<&str> {
        self.api_key.as_deref().ok_or_else(|| anyhow!("GOOGLE_AI_API_KEY not configured"))
    }

    // ── Gemini text/vision chat ───────────────────────────────────────────────

    /// Ask Gemini a question — optionally with an image (base64 or URL)
    pub async fn gemini_chat(
        &self,
        prompt: &str,
        model: &str,
        image_url: Option<&str>,
    ) -> Result<String> {
        let key = self.key()?;

        let mut parts = vec![json!({ "text": prompt })];

        // Add image if provided
        if let Some(url) = image_url {
            if url.starts_with("http") {
                // Fetch image and base64-encode it
                let resp = self.http.get(url).send().await?;
                let mime = resp.headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("image/jpeg")
                    .to_string();
                let bytes = resp.bytes().await?;
                let b64 = base64_encode(&bytes);
                parts.push(json!({
                    "inline_data": { "mime_type": mime, "data": b64 }
                }));
            }
        }

        let body = json!({
            "contents": [{ "parts": parts }],
            "generationConfig": {
                "temperature": 0.7,
                "maxOutputTokens": 8192,
            }
        });

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            model, key
        );

        let resp = self.http.post(&url).json(&body).send().await?;
        let status = resp.status();
        let data: Value = resp.json().await?;

        if !status.is_success() {
            return Err(anyhow!("Gemini error {}: {}", status, data));
        }

        let text = data["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .ok_or_else(|| anyhow!("No text in Gemini response: {}", data))?
            .to_string();

        Ok(text)
    }

    /// Long-context research with Gemini 1.5 Pro (1M token context window)
    pub async fn gemini_research(&self, prompt: &str, context: &str) -> Result<String> {
        let full_prompt = if context.is_empty() {
            prompt.to_string()
        } else {
            format!("Context:\n{}\n\nTask: {}", context, prompt)
        };
        self.gemini_chat(&full_prompt, "gemini-1.5-pro", None).await
    }

    // ── Imagen 3 — image generation ──────────────────────────────────────────

    /// Generate image via Gemini image model (generateContent endpoint)
    /// Uses gemini-3.1-flash-image — works with standard Gemini Developer API key.
    pub async fn generate_image(
        &self,
        prompt: &str,
        _aspect_ratio: &str,
        _count: u32,
        output_dir: &str,
    ) -> Result<Vec<String>> {
        let key = self.key()?;
        info!("Generating image: {}", &prompt[..prompt.len().min(80)]);

        let body = json!({
            "contents": [{ "parts": [{ "text": prompt }] }],
            "generationConfig": { "responseModalities": ["IMAGE", "TEXT"] }
        });

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.1-flash-image:generateContent?key={}",
            key
        );

        let resp = self.http.post(&url).json(&body).send().await?;
        let status = resp.status();

        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Gemini image error {}: {}", status, &text[..text.len().min(300)]));
        }

        let data: Value = resp.json().await?;
        self.save_images_gemini(&data, output_dir).await
    }

    /// Parse inlineData images from Gemini generateContent response
    async fn save_images_gemini(&self, data: &Value, output_dir: &str) -> Result<Vec<String>> {
        tokio::fs::create_dir_all(output_dir).await?;
        let mut paths = vec![];

        let parts = data["candidates"]
            .get(0)
            .and_then(|c| c["content"]["parts"].as_array())
            .ok_or_else(|| anyhow!("No parts in Gemini image response: {}", data))?;

        for part in parts {
            if let Some(inline) = part.get("inlineData") {
                let b64 = inline["data"].as_str()
                    .ok_or_else(|| anyhow!("Missing inlineData.data"))?;
                let mime = inline["mimeType"].as_str().unwrap_or("image/jpeg");
                let ext = if mime.contains("png") { "png" } else { "jpg" };
                let bytes = base64_decode(b64)?;
                let filename = format!("{}/frank_img_{}.{}", output_dir, uuid_short(), ext);
                tokio::fs::write(&filename, &bytes).await?;
                info!("Image saved: {}", filename);
                paths.push(filename);
            }
        }

        if paths.is_empty() {
            return Err(anyhow!("No image data in Gemini response"));
        }
        Ok(paths)
    }

    async fn save_images(&self, data: &Value, output_dir: &str) -> Result<Vec<String>> {
        tokio::fs::create_dir_all(output_dir).await?;
        let mut paths = vec![];

        let predictions = data["predictions"].as_array()
            .ok_or_else(|| anyhow!("No predictions in response: {}", data))?;

        for (i, pred) in predictions.iter().enumerate() {
            let b64 = pred["bytesBase64Encoded"]
                .as_str()
                .ok_or_else(|| anyhow!("No image data in prediction {}", i))?;

            let bytes = base64_decode(b64)?;
            let mime = pred["mimeType"].as_str().unwrap_or("image/png");
            let ext = if mime.contains("png") { "png" } else { "jpg" };
            let filename = format!("{}/frank_img_{}.{}", output_dir, uuid_short(), ext);

            tokio::fs::write(&filename, &bytes).await?;
            info!("Image saved: {}", filename);
            paths.push(filename);
        }

        Ok(paths)
    }

    // ── Veo 2 — video generation ──────────────────────────────────────────────

    pub async fn generate_video(
        &self,
        prompt: &str,
        duration_seconds: u32,
        aspect_ratio: &str,
        output_dir: &str,
    ) -> Result<String> {
        let key = self.key()?;
        info!("Generating video: {}", &prompt[..prompt.len().min(80)]);

        // Submit video generation job
        let body = json!({
            "instances": [{
                "prompt": prompt,
            }],
            "parameters": {
                "aspectRatio": aspect_ratio,
                "durationSeconds": duration_seconds.min(8),
                "sampleCount": 1,
            }
        });

        let url = format!(
            "https://us-central1-aiplatform.googleapis.com/v1/{}/publishers/google/models/veo-2.0-generate-001:predictLongRunning?key={}",
            self.project.as_deref().unwrap_or("projects/264277255612"),
            key
        );

        let resp = self.http.post(&url).json(&body).send().await?;
        let status = resp.status();
        let data: Value = resp.json().await?;

        if !status.is_success() {
            return Err(anyhow!("Veo job submission error {}: {}", status, data));
        }

        // Get operation name for polling
        let op_name = data["name"].as_str()
            .ok_or_else(|| anyhow!("No operation name in Veo response: {}", data))?
            .to_string();

        // Poll until complete (up to 5 min)
        let video_data = self.poll_long_running(&op_name, key, 300).await?;

        // Save video
        tokio::fs::create_dir_all(output_dir).await?;
        let b64 = video_data["predictions"][0]["bytesBase64Encoded"]
            .as_str()
            .ok_or_else(|| anyhow!("No video bytes in result"))?;
        let bytes = base64_decode(b64)?;
        let filename = format!("{}/frank_video_{}.mp4", output_dir, uuid_short());
        tokio::fs::write(&filename, &bytes).await?;
        info!("Video saved: {}", filename);

        Ok(filename)
    }

    async fn poll_long_running(&self, op_name: &str, key: &str, timeout_secs: u64) -> Result<Value> {
        let url = format!(
            "https://us-central1-aiplatform.googleapis.com/v1/{}?key={}",
            op_name, key
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        let mut interval = 5u64;

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;

            let resp = self.http.get(&url).send().await?;
            let data: Value = resp.json().await?;

            if data["done"].as_bool().unwrap_or(false) {
                if let Some(err) = data.get("error") {
                    return Err(anyhow!("Long-running operation failed: {}", err));
                }
                return Ok(data["response"].clone());
            }

            if std::time::Instant::now() > deadline {
                return Err(anyhow!("Video generation timed out after {}s", timeout_secs));
            }

            interval = (interval * 2).min(30);
        }
    }

    // ── Image analysis ────────────────────────────────────────────────────────

    pub async fn analyze_image(&self, image_url: &str, question: &str) -> Result<String> {
        let prompt = if question.is_empty() {
            "Describe this image in detail. What do you see?".to_string()
        } else {
            question.to_string()
        };
        self.gemini_chat(&prompt, "gemini-1.5-flash", Some(image_url)).await
    }
}

// ── Tool definitions for Frank ────────────────────────────────────────────────

pub fn google_ai_tools() -> Vec<crate::tools::ToolDef> {
    vec![
        crate::tools::ToolDef {
            name: "generate_image".into(),
            description: "Generate images using Google Imagen 3. Creates photorealistic or artistic images from text prompts. Returns file paths to the generated images.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "Detailed description of the image to generate" },
                    "aspect_ratio": { "type": "string", "description": "Aspect ratio: 1:1 (default), 16:9, 9:16, 4:3, 3:4" },
                    "count": { "type": "integer", "description": "Number of images to generate (1-4, default 1)" }
                },
                "required": ["prompt"]
            }),
        },
        crate::tools::ToolDef {
            name: "generate_video".into(),
            description: "Generate short videos using Google Veo 2. Creates MP4 videos from text prompts. Takes 1-3 minutes. Returns file path when complete.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "Detailed description of the video to generate" },
                    "duration_seconds": { "type": "integer", "description": "Duration in seconds (1-8, default 5)" },
                    "aspect_ratio": { "type": "string", "description": "Aspect ratio: 16:9 (default), 9:16, 1:1" }
                },
                "required": ["prompt"]
            }),
        },
        crate::tools::ToolDef {
            name: "analyze_image".into(),
            description: "Analyze or describe an image using Google Gemini Vision. Can answer questions about image content, read text in images, identify objects, etc.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "image_url": { "type": "string", "description": "URL of the image to analyze" },
                    "question": { "type": "string", "description": "What to ask about the image (optional — defaults to full description)" }
                },
                "required": ["image_url"]
            }),
        },
        crate::tools::ToolDef {
            name: "gemini_research".into(),
            description: "Use Gemini 1.5 Pro for deep research and long-context reasoning. Good for analyzing large documents, complex multi-step reasoning, or when you need a second opinion from a different model.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "The research question or task" },
                    "context": { "type": "string", "description": "Additional context, documents, or data to include (optional)" }
                },
                "required": ["prompt"]
            }),
        },
        crate::tools::ToolDef {
            name: "gemini_chat".into(),
            description: "Chat with Gemini Flash for fast, cheap responses. Use for summarization, formatting, classification, or quick questions that don't need Claude.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "The message or question" },
                    "model": { "type": "string", "description": "Model: gemini-1.5-flash (default, fast), gemini-1.5-pro (deep)" }
                },
                "required": ["prompt"]
            }),
        },
    ]
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn base64_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i] as u32;
        let b1 = if i + 1 < bytes.len() { bytes[i+1] as u32 } else { 0 };
        let b2 = if i + 2 < bytes.len() { bytes[i+2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((n >> 18) & 63) as usize] as char);
        out.push(CHARS[((n >> 12) & 63) as usize] as char);
        out.push(if i + 1 < bytes.len() { CHARS[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if i + 2 < bytes.len() { CHARS[(n & 63) as usize] as char } else { '=' });
        i += 3;
    }
    out
}

fn base64_decode(s: &str) -> Result<Vec<u8>> {
    let s = s.trim().replace('\n', "").replace('\r', "");
    let mut out = Vec::new();
    let chars: Vec<u8> = s.bytes().collect();
    let mut i = 0;
    let decode_char = |c: u8| -> Result<u32> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
            b'+' => Ok(62),
            b'/' => Ok(63),
            b'=' => Ok(0),
            _ => Err(anyhow!("Invalid base64 char: {}", c)),
        }
    };
    while i + 3 < chars.len() {
        let n = (decode_char(chars[i])? << 18)
              | (decode_char(chars[i+1])? << 12)
              | (decode_char(chars[i+2])? << 6)
              | decode_char(chars[i+3])?;
        out.push(((n >> 16) & 0xff) as u8);
        if chars[i+2] != b'=' { out.push(((n >> 8) & 0xff) as u8); }
        if chars[i+3] != b'=' { out.push((n & 0xff) as u8); }
        i += 4;
    }
    Ok(out)
}

fn uuid_short() -> String {
    let id = uuid::Uuid::new_v4().to_string();
    id[..8].to_string()
}
