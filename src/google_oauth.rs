//! Google OAuth 2.0 — Sign in with Google
//! Flow: /auth/google → Google → /auth/google/callback → JWT

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Json, Redirect},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{auth, AppState};

pub fn google_router() -> Router<AppState> {
    Router::new()
        .route("/auth/google",          get(google_login))
        .route("/auth/google/callback", get(google_callback))
}

// ── Step 1: Redirect to Google ────────────────────────────────────────────────

async fn google_login(
    State(state): State<AppState>,
) -> Result<Redirect, (StatusCode, Json<Value>)> {
    let client_id = state.config.google_client_id.as_deref()
        .ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "Google OAuth not configured"}))))?;
    let redirect_uri = state.config.google_redirect_uri.as_deref()
        .unwrap_or("https://frank.swarmlogic.cloud/api/v1/auth/google/callback");

    let url = format!(
        "https://accounts.google.com/o/oauth2/v2/auth\
         ?client_id={client_id}\
         &redirect_uri={redirect_uri}\
         &response_type=code\
         &scope=openid%20email%20profile\
         &access_type=offline\
         &prompt=select_account",
        client_id = urlencoding(client_id),
        redirect_uri = urlencoding(redirect_uri),
    );

    Ok(Redirect::temporary(&url))
}

// ── Step 2: Handle callback from Google ──────────────────────────────────────

#[derive(Deserialize)]
struct CallbackParams {
    code: Option<String>,
    error: Option<String>,
}

async fn google_callback(
    State(state): State<AppState>,
    Query(params): Query<CallbackParams>,
) -> Result<axum::response::Response, (StatusCode, Json<Value>)> {
    use axum::response::IntoResponse;

    if let Some(err) = params.error {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": format!("Google auth error: {}", err)}))));
    }

    let code = params.code.ok_or_else(|| (StatusCode::BAD_REQUEST, Json(json!({"error": "No code returned"}))))?;
    let client_id     = state.config.google_client_id.clone().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "Google not configured"}))))?;
    let client_secret = state.config.google_client_secret.clone().ok_or_else(|| (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "Google not configured"}))))?;
    let redirect_uri  = state.config.google_redirect_uri.clone().unwrap_or_else(|| "https://frank.swarmlogic.cloud/api/v1/auth/google/callback".to_string());

    // Exchange code for tokens
    let google_user = exchange_code_for_user(&code, &client_id, &client_secret, &redirect_uri)
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, Json(json!({"error": format!("Token exchange failed: {}", e)}))))?;

    // Upsert user in DB
    let (user_id, role, relationship, is_master) = upsert_google_user(&state.db, &google_user)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    // Issue JWT
    let token = auth::create_token(&user_id, &google_user.email, &role, &state.config.jwt_secret)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))))?;

    // Redirect to frontend with token in fragment — SPA picks it up
    let frontend_url = format!(
        "https://frank.swarmlogic.cloud/#google_token={}&user_id={}&email={}&name={}&role={}&relationship={}&is_master={}",
        urlencoding(&token),
        user_id,
        urlencoding(&google_user.email),
        urlencoding(&google_user.name),
        role,
        relationship,
        is_master,
    );

    Ok(Redirect::temporary(&frontend_url).into_response())
}

// ── Token exchange ────────────────────────────────────────────────────────────

#[derive(Debug)]
struct GoogleUser {
    email: String,
    name: String,
    picture: Option<String>,
    google_id: String,
}

async fn exchange_code_for_user(
    code: &str,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
) -> anyhow::Result<GoogleUser> {
    let http = reqwest::Client::new();

    // Exchange code for access token
    let token_resp: Value = http
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("code",          code),
            ("client_id",     client_id),
            ("client_secret", client_secret),
            ("redirect_uri",  redirect_uri),
            ("grant_type",    "authorization_code"),
        ])
        .send().await?
        .json().await?;

    let access_token = token_resp["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("No access_token in response: {}", token_resp))?;

    // Fetch user info
    let user_info: Value = http
        .get("https://www.googleapis.com/oauth2/v2/userinfo")
        .bearer_auth(access_token)
        .send().await?
        .json().await?;

    Ok(GoogleUser {
        email:     user_info["email"].as_str().unwrap_or("").to_string(),
        name:      user_info["name"].as_str().unwrap_or("").to_string(),
        picture:   user_info["picture"].as_str().map(String::from),
        google_id: user_info["id"].as_str().unwrap_or("").to_string(),
    })
}

// ── Upsert user ───────────────────────────────────────────────────────────────

async fn upsert_google_user(
    pool: &sqlx::PgPool,
    user: &GoogleUser,
) -> anyhow::Result<(Uuid, String, String, bool)> {
    use sqlx::Row;

    // Add google_id column if not present (safe migration)
    let _ = sqlx::query("ALTER TABLE frankos_users ADD COLUMN IF NOT EXISTS google_id TEXT")
        .execute(pool).await;
    let _ = sqlx::query("ALTER TABLE frankos_users ADD COLUMN IF NOT EXISTS avatar_url TEXT")
        .execute(pool).await;

    // Check if user exists
    let existing = sqlx::query(
        "SELECT id, role, relationship, is_master_user FROM frankos_users WHERE email = $1"
    )
    .bind(&user.email)
    .fetch_optional(pool).await?;

    if let Some(row) = existing {
        // Update google_id + avatar if missing
        let _ = sqlx::query(
            "UPDATE frankos_users SET google_id = $1, avatar_url = COALESCE(avatar_url, $2), updated_at = NOW() WHERE email = $3"
        )
        .bind(&user.google_id).bind(&user.picture).bind(&user.email)
        .execute(pool).await;

        let id: Uuid = row.try_get("id")?;
        let role: String = row.try_get("role").unwrap_or_else(|_| "user".into());
        let rel: String  = row.try_get("relationship").unwrap_or_else(|_| "user".into());
        let master: bool = row.try_get("is_master_user").unwrap_or(false);
        return Ok((id, role, rel, master));
    }

    // New user — create account
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO frankos_users (email, name, google_id, avatar_url, role, relationship, is_master_user)
         VALUES ($1, $2, $3, $4, 'user', 'user', false) RETURNING id"
    )
    .bind(&user.email).bind(&user.name).bind(&user.google_id).bind(&user.picture)
    .fetch_one(pool).await?;

    Ok((id, "user".into(), "user".into(), false))
}

// ── Tiny URL encoder ──────────────────────────────────────────────────────────

fn urlencoding(s: &str) -> String {
    s.chars().map(|c| match c {
        'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
        _ => format!("%{:02X}", c as u32),
    }).collect()
}
