//! FrankOS Gateway — SuperFrank v2.0 — Superpowers Edition

use anyhow::Result;
use axum::{
    extract::State,
    response::Json,
    routing::get,
    Router,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod agents;
mod admin;
mod auth;
mod auto_memory;
mod db;
mod extractor;
mod identity;
mod llm;
mod memory;
mod routes;
mod sse;
mod system_events;
mod tools;
mod tool_registry;
mod voice;
mod google_oauth;
mod google_ai;
mod luma;
// v3
mod nexus;
mod forge;
mod swarm;
mod delivery;
mod forge_tools;
mod goals_tools;
mod skills_tools;
mod embeddings;
mod semantic_search;
mod worker;
mod events;
mod plan_continuation;

#[derive(Clone)]
pub struct AppState {
    pub db: sqlx::PgPool,
    pub config: Arc<Config>,
    pub llm: Arc<llm::LlmClient>,
    pub google_ai: Arc<google_ai::GoogleAiClient>,
    pub luma: Arc<luma::LumaClient>,
    // v3
    pub forge: Arc<forge::Forge>,
    pub swarm: Arc<swarm::Swarm>,
    pub delivery: Arc<delivery::DeliveryBus>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub jwt_secret: String,
    pub host: String,
    pub port: u16,
    pub frankos_version: String,
    pub anthropic_api_key: Option<String>,
    pub openai_api_key: Option<String>,
    pub brave_api_key: Option<String>,
    pub google_client_id: Option<String>,
    pub google_client_secret: Option<String>,
    pub google_redirect_uri: Option<String>,
    pub google_ai_api_key: Option<String>,
    pub google_ai_project: Option<String>,
    pub luma_api_key: Option<String>,
    pub resend_api_key: Option<String>,
    pub frank_from_email: Option<String>,
    // Hub
    pub hub_supabase_url: Option<String>,
    pub hub_supabase_service_key: Option<String>,
    pub hub_admin_token: Option<String>,
    pub hub_pm2: String,
    pub hub_node: String,
}

impl Config {
    pub fn from_env() -> Self {
        dotenv::dotenv().ok();
        Self {
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "postgresql://frankos@localhost/frankos".to_string()),
            jwt_secret: std::env::var("JWT_SECRET").expect("JWT_SECRET must be set"),
            host: std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
            port: std::env::var("PORT").unwrap_or_else(|_| "8080".to_string()).parse().unwrap_or(8080),
            frankos_version: env!("CARGO_PKG_VERSION").to_string(),
            anthropic_api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
            openai_api_key: std::env::var("OPENAI_API_KEY").ok(),
            brave_api_key: std::env::var("BRAVE_API_KEY").ok(),
            google_client_id: std::env::var("GOOGLE_CLIENT_ID").ok(),
            google_client_secret: std::env::var("GOOGLE_CLIENT_SECRET").ok(),
            google_redirect_uri: std::env::var("GOOGLE_REDIRECT_URI").ok(),
            google_ai_api_key: std::env::var("GOOGLE_AI_API_KEY").ok(),
            google_ai_project: std::env::var("GOOGLE_AI_PROJECT").ok(),
            luma_api_key: std::env::var("LUMALABS_API_KEY").ok().or_else(|| std::env::var("LUMA_API_KEY").ok()),
            resend_api_key: std::env::var("RESEND_API_KEY").ok(),
            frank_from_email: std::env::var("FRANK_FROM_EMAIL").ok(),
            hub_supabase_url: std::env::var("HUB_SUPABASE_URL").ok(),
            hub_supabase_service_key: std::env::var("HUB_SUPABASE_SERVICE_KEY").ok(),
            hub_admin_token: std::env::var("HUB_ADMIN_TOKEN").ok(),
            hub_pm2: std::env::var("HUB_PM2").unwrap_or_else(|_| "/root/.nvm/versions/node/v22.23.2/bin/pm2".to_string()),
            hub_node: std::env::var("HUB_NODE").unwrap_or_else(|_| "/root/.nvm/versions/node/v22.23.2/bin/node".to_string()),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "frankos_gateway=info,tower_http=warn".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Config::from_env();
    info!("FrankOS Gateway {} starting — SUPERPOWERS EDITION", identity::FRANK_VERSION);
    info!("Anthropic: {}", if config.anthropic_api_key.is_some() { "✓" } else { "✗" });
    info!("OpenAI:    {}", if config.openai_api_key.is_some() { "✓" } else { "✗" });
    info!("Brave:     {}", if config.brave_api_key.is_some() { "✓ web search ready" } else { "✗ no search" });

    let pool = sqlx::PgPool::connect(&config.database_url).await?;
    info!("Database connected");
    db::run_migrations(&pool).await?;

    let google_ai = Arc::new(google_ai::GoogleAiClient::new(
        config.google_ai_api_key.clone(),
        config.google_ai_project.clone(),
    ));

    let llm = Arc::new(llm::LlmClient::new(
        config.anthropic_api_key.clone(),
        config.openai_api_key.clone(),
    ));

    let luma = Arc::new(luma::LumaClient::new(config.luma_api_key.clone()));

    // v3 components
    let forge = Arc::new(forge::Forge::new());
    let swarm_inst = Arc::new(swarm::Swarm::new(pool.clone(), llm.clone()));
    let delivery = Arc::new(delivery::DeliveryBus::new(
        pool.clone(),
        config.resend_api_key.clone(),
        config.frank_from_email.clone(),
    ));

    // Run v3 DB migrations
    db::run_v3_migrations(&pool).await?;
    db::run_v4_migrations(&pool).await?;
    db::run_v5_migrations(&pool).await?;
    db::run_v6_migrations(&pool).await?;
    db::run_v7_migrations(&pool).await?;
    db::run_v8_migrations(&pool).await?;
    db::run_v9_migrations(&pool).await?;
    db::run_v10_migrations(&pool).await?;

    let state = AppState {
        db: pool.clone(),
        google_ai,
        luma,
        config: Arc::new(config.clone()),
        llm: llm.clone(),
        forge: forge.clone(),
        swarm: swarm_inst.clone(),
        delivery: delivery.clone(),
    };

    // Start the Nexus in background
    let nexus = nexus::Nexus::new(pool.clone(), swarm_inst.clone(), delivery.clone());
    tokio::spawn(async move { nexus.run().await });
    info!("Nexus started");
    // Start agent worker loop — picks up spawned agents without needing an active chat turn
    let worker_pool = pool.clone();
    let worker_llm = llm.clone();
    let worker_brave = config.brave_api_key.clone();
    tokio::spawn(worker::run_agent_worker(worker_pool, worker_llm, worker_brave));

    // Periodic Forge reaper (clean up old exited processes every 10 min)
    let forge_reaper = forge.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(600));
        loop { interval.tick().await; forge_reaper.reap_old(3600).await; }
    });

    let cors = CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any);

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/api/v1/status", get(status_handler))
        .nest("/api/v1", routes::api_router())
        .nest("/api/v1", sse::sse_router())
        .nest("/api/v1", admin::admin_router())
        .nest("/api/v1", voice::voice_router())
        .nest("/api/v1", google_oauth::google_router())
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = format!("{}:{}", config.host, config.port);
    info!("Listening on http://{}", addr);
    let listener = TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health_handler() -> Json<Value> {
    Json(json!({ "status": "ok", "service": "frankos-gateway", "version": identity::FRANK_VERSION }))
}

async fn status_handler(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "service": "frankos-gateway",
        "version": identity::FRANK_VERSION,
        "superpowers": true,
        "llm": {
            "anthropic": state.llm.anthropic_key.is_some(),
            "openai": state.llm.openai_key.is_some(),
        },
        "tools": {
            "web_search": state.config.brave_api_key.is_some(),
            "shell_exec": true,
            "file_ops": true,
            "git": true,
            "memory": true,
            "agent_swarm": true,
        }
    }))
}
