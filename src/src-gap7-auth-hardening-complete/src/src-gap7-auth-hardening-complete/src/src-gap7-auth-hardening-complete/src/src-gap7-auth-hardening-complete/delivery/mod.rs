//! The Delivery Bus — proactive reach to Chuck.
//!
//! Frank can now reach Chuck anywhere, not just when he has the chat window open.
//! Current targets: in-session SSE, email (Resend), webhooks.
//! Future: web push (VAPID), mobile push when the native app exists.

use anyhow::Result;
use reqwest::Client;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{info, warn};
use uuid::Uuid;

use crate::llm::StreamEvent;

// ── Session registry — maps session IDs to live SSE senders ──────────────────

#[derive(Clone, Default)]
pub struct SessionRegistry {
    /// session_id → SSE event sender
    senders: Arc<RwLock<HashMap<Uuid, mpsc::Sender<StreamEvent>>>>,
}

impl SessionRegistry {
    pub async fn register(&self, session_id: Uuid, tx: mpsc::Sender<StreamEvent>) {
        self.senders.write().await.insert(session_id, tx);
    }

    pub async fn unregister(&self, session_id: Uuid) {
        self.senders.write().await.remove(&session_id);
    }

    pub async fn send(&self, session_id: Uuid, event: StreamEvent) -> bool {
        let senders = self.senders.read().await;
        if let Some(tx) = senders.get(&session_id) {
            tx.send(event).await.is_ok()
        } else {
            false
        }
    }

    pub async fn active_count(&self) -> usize {
        self.senders.read().await.len()
    }
}

// ── DeliveryBus ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct DeliveryBus {
    pub sessions: Arc<SessionRegistry>,
    http: Client,
    db: PgPool,
    resend_key: Option<String>,
    from_email: String,
}

impl DeliveryBus {
    pub fn new(
        db: PgPool,
        resend_key: Option<String>,
        from_email: Option<String>,
    ) -> Self {
        Self {
            sessions: Arc::new(SessionRegistry::default()),
            http: Client::new(),
            db,
            resend_key,
            from_email: from_email.unwrap_or_else(|| "frank@swarmlogic.cloud".to_string()),
        }
    }

    /// Notify a user through the internal bus.
    /// Behavior:
    /// 1) Persist in frank_notifications
    /// 2) Attempt live SSE delivery
    /// 3) Keep queued for later retrieval if no active session
    pub async fn notify_user(&self, user_id: Uuid, title: &str, body: &str) -> Result<()> {
        let notif_id = self.store_notification(user_id, "delivery_bus", "info", title, body, None).await?;

        // Find the user's most recent active session
        let session_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM frankos_sessions
             WHERE user_id = $1 AND expires_at > NOW() AND revoked_at IS NULL
             ORDER BY created_at DESC LIMIT 1"
        ).bind(user_id).fetch_optional(&self.db).await?;

        if let Some(sid) = session_id {
            let delivered = self.sessions.send(
                sid,
                StreamEvent::Notification {
                    title: title.to_string(),
                    body: body.to_string(),
                },
            ).await;

            if delivered {
                self.mark_notification_delivered(notif_id, "sse").await?;
                info!("[Delivery] Notification delivered via SSE to session {}", sid);
                return Ok(());
            }
        }

        info!("[Delivery] Notification queued (no active SSE session) for user {}", user_id);
        Ok(())
    }

    pub async fn store_notification(
        &self,
        user_id: Uuid,
        source: &str,
        level: &str,
        title: &str,
        body: &str,
        metadata: Option<Value>,
    ) -> Result<Uuid> {
        let id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO frank_notifications (user_id, source, level, title, body, metadata, status)
             VALUES ($1, $2, $3, $4, $5, $6, 'queued')
             RETURNING id"
        )
        .bind(user_id)
        .bind(source)
        .bind(level)
        .bind(title)
        .bind(body)
        .bind(metadata)
        .fetch_one(&self.db)
        .await?;

        Ok(id)
    }

    pub async fn mark_notification_delivered(&self, notification_id: Uuid, via: &str) -> Result<()> {
        sqlx::query(
            "UPDATE frank_notifications
             SET status = 'delivered',
                 delivered_via = $2,
                 delivered_at = NOW()
             WHERE id = $1"
        )
        .bind(notification_id)
        .bind(via)
        .execute(&self.db)
        .await?;

        Ok(())
    }

    /// Send email to a user via Resend.
    pub async fn send_email_to_user(&self, user_id: Uuid, subject: &str, body: &str) -> Result<()> {
        let Some(key) = &self.resend_key else {
            warn!("[Delivery] No Resend key — email not sent");
            return Ok(());
        };

        let to_email = sqlx::query_scalar::<_, String>(
            "SELECT email FROM frankos_users WHERE id = $1"
        ).bind(user_id).fetch_optional(&self.db).await?
         .unwrap_or_default();

        if to_email.is_empty() {
            return Ok(());
        }

        self.send_email_raw(key, &to_email, subject, body).await
    }

    pub async fn send_email_raw(
        &self,
        api_key: &str,
        to: &str,
        subject: &str,
        body: &str,
    ) -> Result<()> {
        let html = format!(
            "<div style='font-family:sans-serif;max-width:600px;margin:auto;padding:24px'>\
             <h2 style='color:#1a1a2e'>{}</h2>\
             <p style='color:#333;line-height:1.6'>{}</p>\
             <hr style='border:1px solid #eee;margin:24px 0'/>\
             <p style='color:#999;font-size:12px'>Frank — SwarmLogic</p></div>",
            subject,
            body.replace('\n', "<br>")
        );

        let payload = json!({
            "from": format!("Frank <{}>", self.from_email),
            "to": [to],
            "subject": subject,
            "html": html,
        });

        let resp = self.http
            .post("https://api.resend.com/emails")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&payload)
            .send().await?;

        if resp.status().is_success() {
            info!("[Delivery] Email sent to {}: '{}'", to, subject);
        } else {
            let err = resp.text().await?;
            warn!("[Delivery] Resend error: {}", err);
        }

        Ok(())
    }

    /// Fire an outbound webhook.
    pub async fn fire_webhook(
        &self,
        url: &str,
        secret: Option<&str>,
        body: &Value,
    ) -> Result<()> {
        let mut req = self.http.post(url).json(body);

        if let Some(s) = secret {
            req = req.header("X-Frank-Signature", s);
        }

        let resp = req.send().await?;
        if resp.status().is_success() {
            info!("[Delivery] Webhook fired to {}", url);
        } else {
            warn!("[Delivery] Webhook {} returned {}", url, resp.status());
        }

        Ok(())
    }
}
