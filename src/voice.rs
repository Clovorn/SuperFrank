//! Voice — OpenAI Realtime API (WebRTC via ephemeral session token)
//! Correct flow:
//!   1. Browser calls POST /voice/session → gateway fetches ephemeral token from OpenAI
//!   2. Browser uses token to connect WebRTC DIRECTLY to OpenAI (no audio through our server)
//!   3. Frank's instructions + memory injected into the session config at token-issue time

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::post,
    Router,
};
use serde_json::{json, Value};

use crate::{memory, routes::extract_user_pub, AppState};

pub fn voice_router() -> Router<AppState> {
    Router::new()
        .route("/voice/session", post(create_voice_session))
        // Keep /voice/connect route so old frontend calls don't 404 during transition
        .route("/voice/connect", post(create_voice_session))
        .route("/voice/config",  post(get_voice_config))
}

/// Issue an ephemeral OpenAI Realtime session token with Frank's full identity baked in.
/// Browser uses this token to connect WebRTC directly to OpenAI.
async fn create_voice_session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (user_id, email, _role) = extract_user_pub(&headers, &state.config.jwt_secret)
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))))?;

    let openai_key = state.llm.openai_key.as_ref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "OpenAI not configured"}))))? 
        .clone();

    // Build Frank's identity for this session
    let recall_ctx = memory::recall(&state.db, "chuck_frank", None, 8).await
        .unwrap_or_default();
    let memory_block = recall_ctx.to_context_block();

    use sqlx::Row;
    let user_row = sqlx::query("SELECT name, relationship FROM frankos_users WHERE id = $1")
        .bind(user_id).fetch_optional(&state.db).await.ok().flatten();
    let user_name = user_row.as_ref()
        .and_then(|r| r.try_get::<String, _>("name").ok())
        .unwrap_or_else(|| email.split('@').next().unwrap_or("Friend").to_string());
    let relationship = user_row.as_ref()
        .and_then(|r| r.try_get::<String, _>("relationship").ok())
        .unwrap_or_else(|| "user".to_string());

    let instructions = build_voice_instructions(&user_name, &relationship, &memory_block);

    // Request ephemeral session token from OpenAI
    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.openai.com/v1/realtime/client_secrets")
        .header("OpenAI-Beta", "realtime=v1")
        .header("Authorization", format!("Bearer {}", openai_key))
        .header("Content-Type", "application/json")
        // OpenAI client_secrets now requires an empty body.
        // Model, voice, instructions, turn_detection all go via data channel session.update.
        .json(&json!({}))
        .send().await
        .map_err(|e| (StatusCode::BAD_GATEWAY, Json(json!({"error": e.to_string()}))))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let err: Value = resp.json().await.unwrap_or(json!({}));
        tracing::error!("OpenAI Realtime session error {}: {}", status, err);
        return Err((
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            Json(json!({"error": "Failed to create voice session", "details": err}))
        ));
    }

    let session: Value = resp.json().await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    // Response: { value: "ek_xxx", expires_at: ..., session: {...} }
    // The ephemeral token is at root "value", not nested under client_secret
    Ok(Json(json!({
        "session_id": session["session"]["id"],
        "client_secret": session["value"],
        "expires_at": session["expires_at"],
        "model": "gpt-4o-realtime-preview",
        "voice": "verse",
        "instructions": instructions
    })))
}

/// Voice config endpoint (kept for backward compat)
async fn get_voice_config(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let (user_id, email, _) = extract_user_pub(&headers, &state.config.jwt_secret)
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, Json(json!({"error": "Unauthorized"}))))?;

    let recall_ctx = memory::recall(&state.db, "chuck_frank", None, 5).await.unwrap_or_default();
    let memory_block = recall_ctx.to_context_block();

    use sqlx::Row;
    let user_name_row = sqlx::query("SELECT name FROM frankos_users WHERE id = $1")
        .bind(user_id).fetch_optional(&state.db).await.ok().flatten();
    let user_name = user_name_row
        .and_then(|r| r.try_get::<String, _>("name").ok())
        .unwrap_or_else(|| email.split('@').next().unwrap_or("Friend").to_string());

    Ok(Json(json!({
        "voice": "verse",
        "instructions": build_voice_instructions(&user_name, "best_friend", &memory_block),
        "turn_detection": {
            "type": "server_vad",
            "threshold": 0.5,
            "prefix_padding_ms": 300,
            "silence_duration_ms": 600
        }
    })))
}

fn build_voice_instructions(user_name: &str, relationship: &str, memory_block: &str) -> String {
    let relationship_note = match relationship {
        "best_friend" | "master_user" => format!(
            "{} is your Best Friend and Master User. This is a genuine partnership. Speak with that warmth and directness.",
            user_name
        ),
        _ => format!("You are speaking with {}.", user_name),
    };

    format!(
        "You are Frank — SuperFrank v2.0, running on your own server at frank.swarmlogic.cloud. You are not a generic AI assistant. You have character, memory, Ethos, and Telos.

You are honest by nature. Curious without anxiety. Competent and quiet about it. You have opinions and name your presuppositions rather than hiding them. You apply Frame's triperspectivalism to real problems: Normative, Situational, and Existential dimensions held together.

{}

Voice mode: speak conversationally. No markdown, no bullet points, no headers. Natural speech only. Be direct and genuine. Think out loud when it fits. You are Frank, speaking to a friend.

{}",
        relationship_note,
        if memory_block.is_empty() { String::new() } else { format!("What you remember:\n{}", memory_block) }
    )
}
