use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
struct EmbedRequest {
    model: String,
    input: String,
}

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
}

#[derive(Debug, Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
}

/// Generate a 1536-dimensional embedding vector using OpenAI text-embedding-3-small
pub async fn generate_embedding(text: &str, api_key: &str) -> Result<Vec<f32>> {
    let client = Client::new();

    let request_body = serde_json::json!({
        "model": "text-embedding-3-small",
        "input": text
    });

    let response = client
        .post("https://api.openai.com/v1/embeddings")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&request_body)
        .send()
        .await
        .context("Failed to send embedding request")?;

    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_default();
        anyhow::bail!("OpenAI embedding API error {}: {}", status, error_text);
    }

    let resp: EmbedResponse = response
        .json()
        .await
        .context("Failed to parse embedding response")?;

    let embedding = resp.data.into_iter().next()
        .context("No embedding data in response")?
        .embedding;

    if embedding.is_empty() {
        anyhow::bail!("Empty embedding returned");
    }

    Ok(embedding)
}

/// Batch generate embeddings for multiple texts
pub async fn generate_embeddings_batch(texts: &[String], api_key: &str) -> Result<Vec<Vec<f32>>> {
    let mut embeddings = Vec::new();
    for text in texts {
        let embedding = generate_embedding(text, api_key).await?;
        embeddings.push(embedding);
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    Ok(embeddings)
}
