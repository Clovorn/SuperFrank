//! Database setup and migrations

use anyhow::Result;
use sqlx::PgPool;
use tracing::info;

pub async fn run_migrations(pool: &PgPool) -> Result<()> {
    info!("Running database migrations...");

    // Core tables
    sqlx::query("
        CREATE TABLE IF NOT EXISTS frankos_users (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            email TEXT UNIQUE NOT NULL,
            name TEXT NOT NULL,
            password_hash TEXT,
            role TEXT NOT NULL DEFAULT 'user',
            relationship TEXT NOT NULL DEFAULT 'user',
            is_master_user BOOLEAN NOT NULL DEFAULT FALSE,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
    ").execute(pool).await?;

    sqlx::query("
        CREATE TABLE IF NOT EXISTS frankos_sessions (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            user_id UUID REFERENCES frankos_users(id) ON DELETE CASCADE,
            token_hash TEXT NOT NULL,
            title TEXT,
            bucket TEXT NOT NULL DEFAULT 'personal',
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            expires_at TIMESTAMPTZ NOT NULL,
            revoked_at TIMESTAMPTZ
        )
    ").execute(pool).await?;

    sqlx::query("
        CREATE TABLE IF NOT EXISTS frankos_messages (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            session_id UUID REFERENCES frankos_sessions(id) ON DELETE CASCADE,
            user_id UUID REFERENCES frankos_users(id) ON DELETE CASCADE,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            metadata JSONB,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
    ").execute(pool).await?;

    sqlx::query("
        CREATE TABLE IF NOT EXISTS frankos_memory (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            bucket TEXT NOT NULL DEFAULT 'personal_work',
            namespace TEXT NOT NULL DEFAULT 'chuck_frank',
            memory_type TEXT NOT NULL DEFAULT 'concept',
            title TEXT NOT NULL,
            content TEXT NOT NULL,
            importance INTEGER NOT NULL DEFAULT 5,
            tags TEXT[] DEFAULT '{}',
            embedding JSONB,
            source_url TEXT,
            session_id UUID,
            source TEXT NOT NULL DEFAULT 'manual',
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
    ").execute(pool).await?;

    // Agent swarm tables
    sqlx::query("
        CREATE TABLE IF NOT EXISTS frankos_agents (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            name TEXT NOT NULL,
            goal TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            tools_allowed JSONB NOT NULL DEFAULT '[]',
            model TEXT NOT NULL DEFAULT 'haiku',
            result TEXT,
            error TEXT,
            iterations INTEGER NOT NULL DEFAULT 0,
            parent_session_id UUID REFERENCES frankos_sessions(id) ON DELETE CASCADE,
            user_id UUID REFERENCES frankos_users(id) ON DELETE CASCADE,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            started_at TIMESTAMPTZ,
            completed_at TIMESTAMPTZ
        )
    ").execute(pool).await?;

    sqlx::query("
        CREATE TABLE IF NOT EXISTS frankos_agent_tool_calls (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            agent_id UUID NOT NULL REFERENCES frankos_agents(id) ON DELETE CASCADE,
            tool_name TEXT NOT NULL,
            input JSONB,
            output JSONB,
            success BOOLEAN,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            completed_at TIMESTAMPTZ
        )
    ").execute(pool).await?;

    sqlx::query("
        CREATE TABLE IF NOT EXISTS frankos_procedures (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            name TEXT UNIQUE NOT NULL,
            description TEXT NOT NULL,
            steps JSONB NOT NULL DEFAULT '[]',
            tags TEXT[] DEFAULT '{}',
            use_count INTEGER NOT NULL DEFAULT 0,
            last_used_at TIMESTAMPTZ,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
    ").execute(pool).await?;

    // Indexes
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_session ON frankos_messages(session_id)").execute(pool).await;
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_user ON frankos_messages(user_id)").execute(pool).await;
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_agents_status ON frankos_agents(status)").execute(pool).await;
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_memory_namespace ON frankos_memory(namespace)").execute(pool).await;

    info!("Migrations complete");
    Ok(())
}

// ── v3 migrations ─────────────────────────────────────────────────────────────

pub async fn run_v3_migrations(pool: &PgPool) -> Result<()> {
    info!("Running v3 migrations...");

    // Nexus: trigger/schedule store
    sqlx::query("
        CREATE TABLE IF NOT EXISTS frank_triggers (
            id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            name         TEXT NOT NULL,
            schedule     JSONB NOT NULL,
            payload      JSONB NOT NULL,
            user_id      UUID REFERENCES frankos_users(id) ON DELETE CASCADE,
            enabled      BOOL NOT NULL DEFAULT true,
            fire_count   INT NOT NULL DEFAULT 0,
            max_fires    INT NOT NULL DEFAULT 0,
            next_fire_at TIMESTAMPTZ,
            last_fired   TIMESTAMPTZ,
            created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
    ").execute(pool).await?;

    let _ = sqlx::query("
        CREATE INDEX IF NOT EXISTS idx_triggers_due
            ON frank_triggers(enabled, next_fire_at)
            WHERE enabled = true
    ").execute(pool).await;

    // Swarm: extend agents table
    let _ = sqlx::query("ALTER TABLE frankos_agents ADD COLUMN IF NOT EXISTS swarm_id UUID").execute(pool).await;
    let _ = sqlx::query("ALTER TABLE frankos_agents ADD COLUMN IF NOT EXISTS token_budget INT DEFAULT 80000").execute(pool).await;
    let _ = sqlx::query("ALTER TABLE frankos_agents ADD COLUMN IF NOT EXISTS tokens_used INT DEFAULT 0").execute(pool).await;

    // Agent mailbox
    sqlx::query("
        CREATE TABLE IF NOT EXISTS frank_agent_mailbox (
            id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            from_agent  UUID,
            to_agent    UUID NOT NULL,
            subject     TEXT NOT NULL,
            body        JSONB NOT NULL,
            read        BOOL NOT NULL DEFAULT false,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
    ").execute(pool).await?;
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_mailbox_to ON frank_agent_mailbox(to_agent, read)").execute(pool).await;

    // Push notification subscriptions
    sqlx::query("
        CREATE TABLE IF NOT EXISTS frank_push_subscriptions (
            id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            user_id    UUID REFERENCES frankos_users(id) ON DELETE CASCADE,
            endpoint   TEXT NOT NULL,
            p256dh     TEXT NOT NULL,
            auth       TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            UNIQUE(user_id, endpoint)
        )
    ").execute(pool).await?;

    // Google OAuth columns (safe ADD IF NOT EXISTS)
    let _ = sqlx::query("ALTER TABLE frankos_users ADD COLUMN IF NOT EXISTS google_id TEXT").execute(pool).await;
    let _ = sqlx::query("ALTER TABLE frankos_users ADD COLUMN IF NOT EXISTS avatar_url TEXT").execute(pool).await;

    info!("v3 migrations complete");
    Ok(())
}

// ── v4 migrations — Gap 2: Goals + Planning ───────────────────────────────────

pub async fn run_v4_migrations(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    tracing::info!("Running v4 migrations...");

    sqlx::query("
        CREATE TABLE IF NOT EXISTS frank_goals (
            id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            user_id      UUID REFERENCES frankos_users(id) ON DELETE CASCADE,
            title        TEXT NOT NULL,
            description  TEXT NOT NULL,
            status       TEXT NOT NULL DEFAULT 'active',
            priority     INTEGER NOT NULL DEFAULT 5,
            context      JSONB,
            created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            completed_at TIMESTAMPTZ
        )
    ").execute(pool).await?;

    sqlx::query("
        CREATE TABLE IF NOT EXISTS frank_plan_steps (
            id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            goal_id      UUID NOT NULL REFERENCES frank_goals(id) ON DELETE CASCADE,
            step_number  INTEGER NOT NULL,
            title        TEXT NOT NULL,
            description  TEXT,
            status       TEXT NOT NULL DEFAULT 'pending',
            notes        TEXT,
            created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            completed_at TIMESTAMPTZ
        )
    ").execute(pool).await?;

    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_goals_status ON frank_goals(status, user_id)").execute(pool).await;
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_plan_steps_goal ON frank_plan_steps(goal_id, step_number)").execute(pool).await;

    tracing::info!("v4 migrations complete");
    Ok(())
}

// ── v5 migrations — Gap 3: Named Persistent Agents ────────────────────────────

pub async fn run_v5_migrations(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    tracing::info!("Running v5 migrations...");

    // Persistent agents table
    sqlx::query("
        CREATE TABLE IF NOT EXISTS frank_persistent_agents (
            id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            name            TEXT UNIQUE NOT NULL,
            role            TEXT NOT NULL,
            system_prompt   TEXT NOT NULL,
            model           TEXT NOT NULL DEFAULT 'haiku',
            tools_allowed   JSONB NOT NULL DEFAULT '[]',
            memory_ns       TEXT NOT NULL,
            status          TEXT NOT NULL DEFAULT 'idle',
            session_id      UUID REFERENCES frankos_sessions(id) ON DELETE SET NULL,
            user_id         UUID REFERENCES frankos_users(id) ON DELETE CASCADE,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            last_active_at  TIMESTAMPTZ
        )
    ").execute(pool).await?;

    // Conversation history for persistent agents
    sqlx::query("
        CREATE TABLE IF NOT EXISTS frank_agent_conversations (
            id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            agent_id    UUID NOT NULL REFERENCES frank_persistent_agents(id) ON DELETE CASCADE,
            role        TEXT NOT NULL,
            content     TEXT NOT NULL,
            metadata    JSONB,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
    ").execute(pool).await?;

    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_agent_convos ON frank_agent_conversations(agent_id, created_at)").execute(pool).await;
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_persistent_agents_status ON frank_persistent_agents(status)").execute(pool).await;

    tracing::info!("v5 migrations complete");
    Ok(())
}

// ── v6 migrations — Gap 4: Skills System ──────────────────────────────────────

pub async fn run_v6_migrations(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    tracing::info!("Running v6 migrations...");

    // Skills table - reusable procedures Frank can learn and execute
    sqlx::query("
        CREATE TABLE IF NOT EXISTS frank_skills (
            id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            name            TEXT UNIQUE NOT NULL,
            description     TEXT NOT NULL,
            steps           JSONB NOT NULL,
            tags            TEXT[] NOT NULL DEFAULT '{}',
            use_count       INTEGER NOT NULL DEFAULT 0,
            last_used_at    TIMESTAMPTZ,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
    ").execute(pool).await?;

    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_skills_name ON frank_skills(name)").execute(pool).await;
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_skills_tags ON frank_skills USING GIN(tags)").execute(pool).await;

    tracing::info!("v6 migrations complete");
    Ok(())
}

// ── v7 migrations — Gap 8A: System Events Instrumentation ─────────────────────

pub async fn run_v7_migrations(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    tracing::info!("Running v7 migrations...");

    // Create system_events table (or ensure it exists with the right columns)
    sqlx::query("
        CREATE TABLE IF NOT EXISTS system_events (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            event_type TEXT NOT NULL,
            severity TEXT NOT NULL DEFAULT 'info',
            payload JSONB NOT NULL DEFAULT '{}',
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
    ").execute(pool).await?;

    // Add payload column if missing (handles pre-existing table with different schema)
    let _ = sqlx::query("ALTER TABLE system_events ADD COLUMN IF NOT EXISTS payload JSONB NOT NULL DEFAULT '{}'").execute(pool).await;
    // Make actor_type optional for simple emits
    let _ = sqlx::query("ALTER TABLE system_events ALTER COLUMN actor_type DROP NOT NULL").execute(pool).await;

    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_system_events_type ON system_events(event_type, created_at DESC)").execute(pool).await;
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_system_events_severity ON system_events(severity, created_at DESC)").execute(pool).await;

    tracing::info!("v7 migrations complete");
    Ok(())
}

// ── v8 migrations — Gap 9: Tool Registry ──────────────────────────────────────

pub async fn run_v8_migrations(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    tracing::info!("Running v8 migrations (Gap 9: Tool Registry)...");

    // Tool registry table — stores certified tools and their specs
    sqlx::query("
        CREATE TABLE IF NOT EXISTS frank_tool_registry (
            tool_id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            name              TEXT NOT NULL,
            version           TEXT NOT NULL,
            spec              JSONB NOT NULL,
            status            TEXT NOT NULL DEFAULT 'active',
            certified_by      TEXT NOT NULL,
            certified_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            health_status     TEXT NOT NULL DEFAULT 'unknown',
            last_health_check TIMESTAMPTZ
        )
    ").execute(pool).await?;

    // Tool health log — audit trail of health checks and issues
    sqlx::query("
        CREATE TABLE IF NOT EXISTS frank_tool_health_log (
            id        UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            tool_id   UUID NOT NULL REFERENCES frank_tool_registry(tool_id) ON DELETE CASCADE,
            status    TEXT NOT NULL,
            message   TEXT,
            checked_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )
    ").execute(pool).await?;

    // Indexes for performance
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_tool_registry_status ON frank_tool_registry(status)").execute(pool).await;
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_tool_registry_name ON frank_tool_registry(name)").execute(pool).await;
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_tool_health_log_tool ON frank_tool_health_log(tool_id, checked_at DESC)").execute(pool).await;
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_tool_health_log_status ON frank_tool_health_log(status, checked_at DESC)").execute(pool).await;

    tracing::info!("v8 migrations complete");
    Ok(())
}

// ── v9 migrations — Gap 10A: Session Wavelength + Compound Tools ─────────────

pub async fn run_v9_migrations(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    tracing::info!("Running v9 migrations (Gap 10A: Session Wavelength + Compound Tools)...");

    // Add importance scoring, message_type, and handoff detection to frankos_messages
    let _ = sqlx::query("ALTER TABLE frankos_messages ADD COLUMN IF NOT EXISTS importance INTEGER NOT NULL DEFAULT 5").execute(pool).await;
    let _ = sqlx::query("ALTER TABLE frankos_messages ADD COLUMN IF NOT EXISTS message_type TEXT NOT NULL DEFAULT 'conversation'").execute(pool).await;
    let _ = sqlx::query("ALTER TABLE frankos_messages ADD COLUMN IF NOT EXISTS is_handoff BOOLEAN NOT NULL DEFAULT false").execute(pool).await;

    // Indexes for wave-aware message loading
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_importance ON frankos_messages (session_id, importance DESC)").execute(pool).await;
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_handoff ON frankos_messages (session_id, is_handoff) WHERE is_handoff = true").execute(pool).await;

    // Migration tracking table for db_migration tool
    sqlx::query("
        CREATE TABLE IF NOT EXISTS frank_schema_migrations (
            name        TEXT PRIMARY KEY,
            applied_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            checksum    TEXT
        )
    ").execute(pool).await?;

    tracing::info!("v9 migrations complete");
    Ok(())
}

// ── v10 migrations — Gap 10B: Escalation Mailbox Wiring ──────────────────────

pub async fn run_v10_migrations(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    tracing::info!("Running v10 migrations (Gap 10B: Escalation Mailbox)...");

    // Migrate existing frank_agent_mailbox to new schema
    // Rename columns to match spec
    let _ = sqlx::query("ALTER TABLE frank_agent_mailbox RENAME COLUMN from_agent TO from_agent_id").execute(pool).await;
    let _ = sqlx::query("ALTER TABLE frank_agent_mailbox RENAME COLUMN to_agent TO to_agent_id").execute(pool).await;
    
    // Add new columns
    let _ = sqlx::query("ALTER TABLE frank_agent_mailbox ADD COLUMN IF NOT EXISTS message_type TEXT NOT NULL DEFAULT 'info'").execute(pool).await;
    let _ = sqlx::query("ALTER TABLE frank_agent_mailbox ADD COLUMN IF NOT EXISTS content TEXT").execute(pool).await;
    let _ = sqlx::query("ALTER TABLE frank_agent_mailbox ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'unread'").execute(pool).await;
    
    // Migrate existing data: body JSONB -> content TEXT
    let _ = sqlx::query("UPDATE frank_agent_mailbox SET content = body::text WHERE content IS NULL").execute(pool).await;
    
    // Drop old columns after migration
    let _ = sqlx::query("ALTER TABLE frank_agent_mailbox DROP COLUMN IF EXISTS body").execute(pool).await;
    let _ = sqlx::query("ALTER TABLE frank_agent_mailbox DROP COLUMN IF EXISTS read").execute(pool).await;
    
    // Make from_agent_id and to_agent_id nullable (NULL = SuperFrank)
    let _ = sqlx::query("ALTER TABLE frank_agent_mailbox ALTER COLUMN from_agent_id DROP NOT NULL").execute(pool).await;
    let _ = sqlx::query("ALTER TABLE frank_agent_mailbox ALTER COLUMN to_agent_id DROP NOT NULL").execute(pool).await;
    
    // Add indexes for mailbox queries
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_mailbox_to_agent ON frank_agent_mailbox (to_agent_id, status)").execute(pool).await;
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_mailbox_status ON frank_agent_mailbox (status, created_at DESC)").execute(pool).await;

    tracing::info!("v10 migrations complete");
    Ok(())
}

pub async fn run_v11_migrations(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    tracing::info!("Running v11 migrations (Phase 1: Task Orchestration Table)...");

    sqlx::query("
        CREATE TABLE IF NOT EXISTS tasks (
            id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            title           TEXT NOT NULL,
            description     TEXT,
            status          TEXT NOT NULL DEFAULT 'PLANNING',
            priority        INTEGER DEFAULT 5,
            assigned_to     TEXT,
            created_at      TIMESTAMPTZ DEFAULT now(),
            updated_at      TIMESTAMPTZ DEFAULT now(),
            completed_at    TIMESTAMPTZ,
            context         TEXT,
            parent_task_id  UUID REFERENCES tasks(id),
            blocked_reason  TEXT,
            result_location TEXT
        )
    ").execute(pool).await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_tasks_status      ON tasks(status)").execute(pool).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_tasks_assigned_to ON tasks(assigned_to)").execute(pool).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_tasks_parent      ON tasks(parent_task_id)").execute(pool).await?;

    tracing::info!("v11 migrations complete");
    Ok(())
}

pub async fn run_v12_migrations(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    tracing::info!("Running v12 migrations (Phase 2: LISTEN/NOTIFY trigger on tasks)...");

    // Function: emit task_change notification with row JSON
    sqlx::query("
        CREATE OR REPLACE FUNCTION notify_task_change()
        RETURNS trigger AS $$
        DECLARE
            payload TEXT;
        BEGIN
            IF TG_OP = 'DELETE' THEN
                payload := json_build_object(
                    'op', TG_OP,
                    'id', OLD.id,
                    'status', OLD.status,
                    'assigned_to', OLD.assigned_to
                )::text;
                PERFORM pg_notify('task_change', payload);
                RETURN OLD;
            ELSE
                payload := json_build_object(
                    'op', TG_OP,
                    'id', NEW.id,
                    'title', NEW.title,
                    'status', NEW.status,
                    'priority', NEW.priority,
                    'assigned_to', NEW.assigned_to,
                    'blocked_reason', NEW.blocked_reason,
                    'result_location', NEW.result_location,
                    'updated_at', NEW.updated_at
                )::text;
                PERFORM pg_notify('task_change', payload);
                RETURN NEW;
            END IF;
        END;
        $$ LANGUAGE plpgsql;
    ").execute(pool).await?;

    // Trigger: fires after INSERT or UPDATE on tasks
    sqlx::query("DROP TRIGGER IF EXISTS task_change_trigger ON tasks").execute(pool).await?;
    sqlx::query("
        CREATE TRIGGER task_change_trigger
        AFTER INSERT OR UPDATE OR DELETE ON tasks
        FOR EACH ROW EXECUTE FUNCTION notify_task_change();
    ").execute(pool).await?;

    tracing::info!("v12 migrations complete — task_change trigger installed");
    Ok(())
}

pub async fn run_v13_migrations(pool: &sqlx::PgPool) -> anyhow::Result<()> {
    tracing::info!("Running v13 migrations (internal notifications bus)...");

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS frank_notifications (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            user_id UUID NOT NULL REFERENCES frankos_users(id) ON DELETE CASCADE,
            source TEXT NOT NULL DEFAULT 'system',
            level TEXT NOT NULL DEFAULT 'info',
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            metadata JSONB,
            status TEXT NOT NULL DEFAULT 'queued',
            delivered_via TEXT,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            delivered_at TIMESTAMPTZ,
            acknowledged_at TIMESTAMPTZ
        )"
    ).execute(pool).await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_frank_notifications_user_status_created
         ON frank_notifications(user_id, status, created_at DESC)"
    ).execute(pool).await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_frank_notifications_source_created
         ON frank_notifications(source, created_at DESC)"
    ).execute(pool).await?;

    tracing::info!("v13 migrations complete");
    Ok(())
}
