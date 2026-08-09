//! LumaLabs Agents API v1
//! Docs: https://docs.agents.lumalabs.ai/
//! Base: https://agents.lumalabs.ai/v1
//! Env:  LUMALABS_API_KEY (or LUMA_API_KEY fallback)

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tracing::info;

const LUMA_API: &str = "https://agents.lumalabs.ai/v1";

pub struct LumaClient {
    http: reqwest::Client,
    pub api_key: Option<String>,
}

/// Returned from video generation calls (kept for tools.rs compat)
pub struct LumaGeneration {
    pub id: String,
    pub state: String,
    pub download_url: Option<String>,
    pub failure_reason: Option<String>,
}

impl LumaClient {
    pub fn new(api_key: Option<String>) -> Self {
        Self { http: reqwest::Client::new(), api_key }
    }

    fn key(&self) -> Result<&str> {
        self.api_key.as_deref()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| anyhow!("LUMALABS_API_KEY not configured"))
    }

    // ── Core async helpers ────────────────────────────────────────────────────

    async fn submit(&self, body: &Value) -> Result<String> {
        let key = self.key()?;
        let resp = self.http
            .post(&format!("{}/generations", LUMA_API))
            .header("Authorization", format!("Bearer {}", key))
            .header("Content-Type", "application/json")
            .json(body)
            .send().await?;
        let status = resp.status();
        let data: Value = resp.json().await?;
        if !status.is_success() {
            let msg = data["detail"].as_str()
                .or_else(|| data["message"].as_str())
                .unwrap_or("unknown error");
            return Err(anyhow!("Luma API error {} {}: {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or(""),
                msg));
        }
        data["id"].as_str().map(|s| s.to_string())
            .ok_or_else(|| anyhow!("No id in Luma response: {}", data))
    }

    async fn poll(&self, generation_id: &str, timeout_secs: u64) -> Result<Value> {
        let key = self.key()?;
        let url = format!("{}/generations/{}", LUMA_API, generation_id);
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_secs(timeout_secs);
        loop {
            if tokio::time::Instant::now() > deadline {
                return Err(anyhow!("Luma generation timed out after {}s", timeout_secs));
            }
            let resp = self.http.get(&url)
                .header("Authorization", format!("Bearer {}", key))
                .send().await?;
            let data: Value = resp.json().await?;
            match data["state"].as_str() {
                Some("completed") => return Ok(data),
                Some("failed") => {
                    let reason = data["failure_reason"].as_str().unwrap_or("unknown");
                    return Err(anyhow!("Luma generation failed: {}", reason));
                }
                _ => tokio::time::sleep(tokio::time::Duration::from_secs(3)).await,
            }
        }
    }

    async fn download(&self, url: &str, output_dir: &str, ext: &str) -> Result<String> {
        tokio::fs::create_dir_all(output_dir).await?;
        let bytes = self.http.get(url).send().await?.bytes().await?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default().subsec_nanos();
        let filename = format!("{}/luma_{}_{}.{}", output_dir, ts, bytes.len(), ext);
        tokio::fs::write(&filename, &bytes).await?;
        info!("Luma asset saved: {}", filename);
        Ok(filename)
    }

    // ── Public API ────────────────────────────────────────────────────────────

    pub async fn text_to_image(
        &self,
        prompt: &str,
        _model: &str,
        aspect_ratio: &str,
        output_dir: &str,
    ) -> Result<String> {
        info!("Luma image: {}", &prompt[..prompt.len().min(80)]);
        let body = json!({ "prompt": prompt, "aspect_ratio": aspect_ratio });
        let id = self.submit(&body).await?;
        info!("Luma image job: {}", id);
        let result = self.poll(&id, 120).await?;
        let url = result["output"][0]["url"].as_str()
            .ok_or_else(|| anyhow!("No output URL in result: {}", result))?;
        self.download(url, output_dir, "jpg").await
    }

    pub async fn text_to_video(
        &self,
        prompt: &str,
        _model: &str,
        resolution: &str,
        duration: &str,
        aspect_ratio: &str,
        _loop_video: bool,
        _concepts: Vec<String>,
        output_dir: &str,
    ) -> Result<LumaGeneration> {
        info!("Luma video: {}", &prompt[..prompt.len().min(80)]);
        let body = json!({
            "model": "ray-3.2",
            "type": "video",
            "prompt": prompt,
            "aspect_ratio": aspect_ratio,
            "video": { "resolution": resolution, "duration": duration }
        });
        let id = self.submit(&body).await?;
        info!("Luma video job: {}", id);
        let result = self.poll(&id, 300).await?;
        let url = result["output"][0]["url"].as_str()
            .ok_or_else(|| anyhow!("No output URL in video result: {}", result))?;
        let path = self.download(url, output_dir, "mp4").await?;
        Ok(LumaGeneration {
            id,
            state: "completed".to_string(),
            download_url: Some(path),
            failure_reason: None,
        })
    }

    pub async fn image_to_video(
        &self,
        prompt: &str,
        start_image_url: &str,
        _end_image_url: Option<&str>,
        _model: &str,
        aspect_ratio: &str,
        _loop_video: bool,
        output_dir: &str,
    ) -> Result<LumaGeneration> {
        info!("Luma image-to-video: {}", &prompt[..prompt.len().min(80)]);
        let body = json!({
            "model": "ray-3.2",
            "type": "video",
            "prompt": prompt,
            "aspect_ratio": aspect_ratio,
            "video": {
                "resolution": "720p",
                "duration": "5s",
                "start_frame": { "type": "image", "url": start_image_url }
            }
        });
        let id = self.submit(&body).await?;
        let result = self.poll(&id, 300).await?;
        let url = result["output"][0]["url"].as_str()
            .ok_or_else(|| anyhow!("No output URL"))?;
        let path = self.download(url, output_dir, "mp4").await?;
        Ok(LumaGeneration {
            id,
            state: "completed".to_string(),
            download_url: Some(path),
            failure_reason: None,
        })
    }

    pub async fn image_reference(
        &self,
        prompt: &str,
        reference_urls: Vec<&str>,
        _weight: f32,
        _model: &str,
        aspect_ratio: &str,
        output_dir: &str,
    ) -> Result<String> {
        info!("Luma image reference: {}", &prompt[..prompt.len().min(80)]);
        let mut body = json!({ "prompt": prompt, "aspect_ratio": aspect_ratio });
        if let Some(first_ref) = reference_urls.first() {
            body["modify_image_ref"] = json!({ "type": "image", "url": first_ref });
        }
        let id = self.submit(&body).await?;
        let result = self.poll(&id, 120).await?;
        let url = result["output"][0]["url"].as_str()
            .ok_or_else(|| anyhow!("No output URL"))?;
        self.download(url, output_dir, "jpg").await
    }

    pub async fn style_reference(
        &self,
        prompt: &str,
        style_url: &str,
        _weight: f32,
        _model: &str,
        aspect_ratio: &str,
        output_dir: &str,
    ) -> Result<String> {
        info!("Luma style reference: {}", &prompt[..prompt.len().min(80)]);
        let body = json!({
            "prompt": prompt,
            "aspect_ratio": aspect_ratio,
            "style_ref": { "type": "image", "url": style_url }
        });
        let id = self.submit(&body).await?;
        let result = self.poll(&id, 120).await?;
        let url = result["output"][0]["url"].as_str()
            .ok_or_else(|| anyhow!("No output URL"))?;
        self.download(url, output_dir, "jpg").await
    }

    pub async fn list_generations(&self, limit: u32) -> Result<Vec<Value>> {
        let key = self.key()?;
        let resp = self.http
            .get(&format!("{}/generations?limit={}", LUMA_API, limit))
            .header("Authorization", format!("Bearer {}", key))
            .send().await?;
        let data: Value = resp.json().await?;
        Ok(data["generations"].as_array().cloned().unwrap_or_default())
    }

    pub async fn list_concepts(&self) -> Result<Vec<String>> {
        Ok(vec![
            "orbit".to_string(), "dolly_in".to_string(), "dolly_out".to_string(),
            "pan_left".to_string(), "pan_right".to_string(),
            "tilt_up".to_string(), "tilt_down".to_string(),
        ])
    }
}

/// Tool definitions for all Luma capabilities
pub fn luma_tools() -> Vec<crate::tools::ToolDef> {
    use serde_json::json;
    vec![
        crate::tools::ToolDef {
            name: "luma_text_to_video".into(),
            description: "Generate a video from a text prompt using Luma Ray 3.2. Takes ~60-90 seconds. Returns a video URL.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt":       { "type": "string", "description": "Detailed video description" },
                    "aspect_ratio": { "type": "string", "description": "16:9 | 9:16 | 1:1", "default": "16:9" },
                    "resolution":   { "type": "string", "description": "540p | 720p | 1080p", "default": "720p" },
                    "duration":     { "type": "string", "description": "5s | 9s", "default": "5s" },
                    "model":        { "type": "string", "description": "ray-3.2 | ray-flash-2", "default": "ray-3.2" }
                },
                "required": ["prompt"]
            }),
        },
        crate::tools::ToolDef {
            name: "luma_text_to_image".into(),
            description: "Generate an image from a text prompt using Luma.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt":       { "type": "string" },
                    "aspect_ratio": { "type": "string", "default": "1:1" },
                    "model":        { "type": "string", "default": "photon-1" }
                },
                "required": ["prompt"]
            }),
        },
        crate::tools::ToolDef {
            name: "luma_image_to_video".into(),
            description: "Animate a still image into a video using Luma Ray 3.2.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt":           { "type": "string" },
                    "start_image_url":  { "type": "string" },
                    "aspect_ratio":     { "type": "string", "default": "16:9" }
                },
                "required": ["prompt", "start_image_url"]
            }),
        },
        crate::tools::ToolDef {
            name: "luma_image_reference".into(),
            description: "Generate an image using reference images for composition/style.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt":         { "type": "string" },
                    "reference_urls": { "type": "array", "items": { "type": "string" } },
                    "aspect_ratio":   { "type": "string", "default": "1:1" }
                },
                "required": ["prompt", "reference_urls"]
            }),
        },
        crate::tools::ToolDef {
            name: "luma_style_reference".into(),
            description: "Generate an image applying the style of a reference image.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "prompt":           { "type": "string" },
                    "style_image_url":  { "type": "string" },
                    "aspect_ratio":     { "type": "string", "default": "1:1" }
                },
                "required": ["prompt", "style_image_url"]
            }),
        },
        crate::tools::ToolDef {
            name: "luma_list_generations".into(),
            description: "List recent Luma generations.".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "limit": { "type": "integer", "default": 10 } }
            }),
        },
        crate::tools::ToolDef {
            name: "luma_list_concepts".into(),
            description: "List available Luma camera movement concepts.".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        },
    ]
}
