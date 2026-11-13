//! Postgres persistence. All SQL in the application lives in this module.
//!
//! The schema is the one the TypeScript service created (version 1), plus a `vault_outbox`
//! table (version 2), so this server can run against an existing HCP database.

use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgRow};
use sqlx::types::Json;
use sqlx::{PgConnection, Postgres, QueryBuilder, Row};

use crate::domain::{ActorType, AuditEvent, CoordinationRequest, State, event, new_id};

const SCHEMA_VERSION: i32 = 2;

/// Serialises concurrent `ensure_schema` calls (two servers starting at once).
const SCHEMA_LOCK_KEY: i64 = 0x4843_5001;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
  version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS coordination_requests (
  request_id      TEXT PRIMARY KEY,
  agent_id        TEXT NOT NULL,
  intent          TEXT NOT NULL,
  urgency         TEXT NOT NULL,
  state           TEXT NOT NULL DEFAULT 'SUBMITTED',
  context_package JSONB NOT NULL,
  response_schema JSONB,
  timeout_policy  JSONB NOT NULL,
  routing_hints   JSONB NOT NULL,
  trace_id        TEXT,
  idempotency_key TEXT UNIQUE,
  responder_id    TEXT NOT NULL,
  response_data   JSONB,
  responded_by    TEXT,
  responded_at    TIMESTAMPTZ,
  submitted_at    TIMESTAMPTZ NOT NULL,
  updated_at      TIMESTAMPTZ NOT NULL,
  timeout_at      TIMESTAMPTZ NOT NULL,
  delivered_at    TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_cr_state        ON coordination_requests(state);
CREATE INDEX IF NOT EXISTS idx_cr_agent_id     ON coordination_requests(agent_id);
CREATE INDEX IF NOT EXISTS idx_cr_responder_id ON coordination_requests(responder_id);
CREATE INDEX IF NOT EXISTS idx_cr_timeout_at   ON coordination_requests(timeout_at);

CREATE TABLE IF NOT EXISTS audit_events (
  event_id    TEXT PRIMARY KEY,
  request_id  TEXT NOT NULL REFERENCES coordination_requests(request_id),
  event_type  TEXT NOT NULL,
  actor       TEXT NOT NULL,
  actor_type  TEXT NOT NULL,
  payload     JSONB NOT NULL DEFAULT '{}',
  created_at  TIMESTAMPTZ NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_audit_request_id ON audit_events(request_id);

CREATE TABLE IF NOT EXISTS api_keys (
  key_id     TEXT PRIMARY KEY,
  key_hash   TEXT NOT NULL UNIQUE,
  agent_id   TEXT NOT NULL,
  label      TEXT NOT NULL,
  scopes     JSONB NOT NULL DEFAULT '[]',
  created_at TIMESTAMPTZ NOT NULL,
  revoked_at TIMESTAMPTZ
);

-- Version 2: audit events waiting to be written to the Obsidian vault. A row is inserted in
-- the same transaction as its audit event and deleted once the vault has it.
CREATE TABLE IF NOT EXISTS vault_outbox (
  event_id   TEXT PRIMARY KEY REFERENCES audit_events(event_id),
  request_id TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL
);
"#;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("invalid state transition: {from} -> {to}")]
    InvalidTransition { from: State, to: State },
    /// Optimistic concurrency: someone else moved the request first.
    #[error("request {request_id} is no longer in state {expected}")]
    Conflict { request_id: String, expected: State },
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

/// A state transition together with the audit event that records it.
#[derive(Debug)]
pub struct Transition<'a> {
    pub request_id: &'a str,
    pub from: State,
    pub to: State,
    pub actor: &'a str,
    pub actor_type: ActorType,
    pub payload: Value,
    pub updates: Updates,
}

/// Columns that may change alongside a transition.
#[derive(Debug, Default)]
pub struct Updates {
    pub response_data: Option<Value>,
    pub responded_by: Option<String>,
    pub responded_at: Option<DateTime<Utc>>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub responder_id: Option<String>,
    pub timeout_at: Option<DateTime<Utc>>,
}

#[derive(Debug)]
pub enum Inserted {
    Created,
    /// The idempotency key was already used; this is the request that owns it.
    Existing(Box<CoordinationRequest>),
}

#[derive(Debug, Default, Clone)]
pub struct RequestFilter {
    pub agent_id: Option<String>,
    pub state: Option<String>,
    pub intent: Option<String>,
    pub urgency: Option<String>,
    pub responder_id: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Debug, Default, Clone)]
pub struct AuditFilter {
    pub request_id: Option<String>,
    pub event_type: Option<String>,
    /// Restrict to events of requests owned by this agent.
    pub agent_id: Option<String>,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Debug, Clone)]
pub struct ApiKey {
    pub key_id: String,
    pub agent_id: String,
    pub label: String,
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl ApiKey {
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == scope)
    }
}

/// An audit event waiting to be written to the vault.
#[derive(Debug, Clone)]
pub struct OutboxEntry {
    pub event_id: String,
    pub request_id: String,
}

pub fn hash_api_key(raw: &str) -> String {
    hex::encode(Sha256::digest(raw.as_bytes()))
}

pub fn generate_api_key() -> String {
    format!("hcp_{}", hex::encode(rand::random::<[u8; 32]>()))
}

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
    /// Whether audit events are queued for the vault. Off when no vault is configured, so
    /// the outbox cannot grow without a consumer.
    outbox: bool,
}

impl Store {
    pub async fn connect(database_url: &str, max_connections: u32, outbox: bool) -> Result<Self, sqlx::Error> {
        let options = PgConnectOptions::from_str(database_url)?;
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options)
            .await?;
        Ok(Self { pool, outbox })
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }

    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1").execute(&self.pool).await.map(drop)
    }

    pub async fn ensure_schema(&self) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(SCHEMA_LOCK_KEY).execute(&mut *tx).await?;
        sqlx::raw_sql(SCHEMA).execute(&mut *tx).await?;

        let version: Option<i32> =
            sqlx::query_scalar("SELECT version FROM schema_version LIMIT 1").fetch_optional(&mut *tx).await?;
        match version {
            None => {
                sqlx::query("INSERT INTO schema_version (version) VALUES ($1)")
                    .bind(SCHEMA_VERSION)
                    .execute(&mut *tx)
                    .await?;
            }
            Some(found) if found < SCHEMA_VERSION => {
                sqlx::query("UPDATE schema_version SET version = $1").bind(SCHEMA_VERSION).execute(&mut *tx).await?;
            }
            Some(found) if found > SCHEMA_VERSION => {
                return Err(sqlx::Error::Configuration(
                    format!("database schema is version {found}; this build understands up to {SCHEMA_VERSION}").into(),
                ));
            }
            Some(_) => {}
        }
        tx.commit().await
    }

    // ── Coordination requests ───────────────────────────────────────────────

    /// Inserts a new request and its `CR_SUBMITTED` audit event atomically. If the
    /// idempotency key is already taken nothing is written and the owning request is returned.
    pub async fn insert_request(&self, cr: &CoordinationRequest) -> Result<Inserted, StoreError> {
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO coordination_requests
               (request_id, agent_id, intent, urgency, state, context_package, response_schema,
                timeout_policy, routing_hints, trace_id, idempotency_key, responder_id,
                submitted_at, updated_at, timeout_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
             ON CONFLICT (idempotency_key) DO NOTHING",
        )
        .bind(&cr.request_id)
        .bind(&cr.agent_id)
        .bind(cr.intent.as_str())
        .bind(cr.urgency.as_str())
        .bind(cr.state.as_str())
        .bind(Json(&cr.context_package))
        .bind(cr.response_schema.as_ref().map(Json))
        .bind(Json(&cr.timeout_policy))
        .bind(Json(&cr.routing_hints))
        .bind(&cr.trace_id)
        .bind(&cr.idempotency_key)
        .bind(&cr.responder_id)
        .bind(cr.submitted_at)
        .bind(cr.updated_at)
        .bind(cr.timeout_at)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if inserted == 0 {
            tx.rollback().await?;
            let key = cr.idempotency_key.as_deref().unwrap_or_default();
            let existing = sqlx::query("SELECT * FROM coordination_requests WHERE idempotency_key = $1")
                .bind(key)
                .fetch_one(&self.pool)
                .await?;
            return Ok(Inserted::Existing(Box::new(request_from_row(&existing)?)));
        }

        let submitted = AuditEvent {
            event_id: new_id(),
            request_id: cr.request_id.clone(),
            event_type: cr.state.audit_event(),
            actor: cr.agent_id.clone(),
            actor_type: ActorType::Agent,
            payload: json!({ "intent": cr.intent, "urgency": cr.urgency }),
            created_at: cr.submitted_at,
        };
        self.insert_audit(&mut tx, &submitted).await?;
        tx.commit().await?;
        Ok(Inserted::Created)
    }

    pub async fn get_request(&self, request_id: &str) -> Result<Option<CoordinationRequest>, sqlx::Error> {
        sqlx::query("SELECT * FROM coordination_requests WHERE request_id = $1")
            .bind(request_id)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(request_from_row)
            .transpose()
    }

    pub async fn list_requests(&self, filter: &RequestFilter) -> Result<Vec<CoordinationRequest>, sqlx::Error> {
        let mut query = QueryBuilder::<Postgres>::new("SELECT * FROM coordination_requests WHERE TRUE");
        let columns = [
            ("agent_id", &filter.agent_id),
            ("state", &filter.state),
            ("intent", &filter.intent),
            ("urgency", &filter.urgency),
            ("responder_id", &filter.responder_id),
        ];
        for (column, value) in columns {
            if let Some(value) = value {
                query.push(" AND ").push(column).push(" = ").push_bind(value.clone());
            }
        }
        query.push(" ORDER BY submitted_at DESC, request_id DESC LIMIT ").push_bind(filter.limit);
        query.push(" OFFSET ").push_bind(filter.offset);
        query.build().fetch_all(&self.pool).await?.iter().map(request_from_row).collect()
    }

    /// Moves a request from one state to another and records the audit event, atomically.
    /// Fails with [`StoreError::Conflict`] if the request is no longer in `from`.
    pub async fn transition(&self, t: Transition<'_>) -> Result<CoordinationRequest, StoreError> {
        if !t.from.can_transition_to(t.to) {
            return Err(StoreError::InvalidTransition { from: t.from, to: t.to });
        }
        let now = Utc::now();
        let mut tx = self.pool.begin().await?;

        let mut update = QueryBuilder::<Postgres>::new("UPDATE coordination_requests SET state = ");
        update.push_bind(t.to.as_str()).push(", updated_at = ").push_bind(now);
        if let Some(value) = t.updates.response_data {
            update.push(", response_data = ").push_bind(Json(value));
        }
        if let Some(value) = t.updates.responded_by {
            update.push(", responded_by = ").push_bind(value);
        }
        if let Some(value) = t.updates.responded_at {
            update.push(", responded_at = ").push_bind(value);
        }
        if let Some(value) = t.updates.delivered_at {
            update.push(", delivered_at = ").push_bind(value);
        }
        if let Some(value) = t.updates.responder_id {
            update.push(", responder_id = ").push_bind(value);
        }
        if let Some(value) = t.updates.timeout_at {
            update.push(", timeout_at = ").push_bind(value);
        }
        update.push(" WHERE request_id = ").push_bind(t.request_id.to_string());
        update.push(" AND state = ").push_bind(t.from.as_str()).push(" RETURNING *");

        let Some(row) = update.build().fetch_optional(&mut *tx).await? else {
            return Err(StoreError::Conflict { request_id: t.request_id.to_string(), expected: t.from });
        };
        let updated = request_from_row(&row)?;

        let entered = AuditEvent {
            event_id: new_id(),
            request_id: t.request_id.to_string(),
            event_type: t.to.audit_event(),
            actor: t.actor.to_string(),
            actor_type: t.actor_type,
            payload: t.payload,
            created_at: now,
        };
        self.insert_audit(&mut tx, &entered).await?;
        tx.commit().await?;
        Ok(updated)
    }

    /// Requests whose deadline has passed and that still need the scheduler's attention.
    pub async fn expired_requests(&self, now: DateTime<Utc>) -> Result<Vec<CoordinationRequest>, sqlx::Error> {
        sqlx::query(
            "SELECT * FROM coordination_requests
             WHERE state IN ('SUBMITTED', 'ROUTING', 'ESCALATED', 'PENDING_RESPONSE') AND timeout_at <= $1
             ORDER BY timeout_at ASC LIMIT 500",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(request_from_row)
        .collect()
    }

    /// Requests that have not reached a human and have not been touched since `stale_before`.
    pub async fn stale_unrouted(
        &self,
        now: DateTime<Utc>,
        stale_before: DateTime<Utc>,
    ) -> Result<Vec<CoordinationRequest>, sqlx::Error> {
        sqlx::query(
            "SELECT * FROM coordination_requests
             WHERE state IN ('SUBMITTED', 'ROUTING', 'ESCALATED') AND timeout_at > $1 AND updated_at <= $2
             ORDER BY updated_at ASC LIMIT 500",
        )
        .bind(now)
        .bind(stale_before)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(request_from_row)
        .collect()
    }

    /// Takes a lease on a stalled request so that only one worker retries its delivery.
    pub async fn claim_stale(
        &self,
        request_id: &str,
        state: State,
        stale_before: DateTime<Utc>,
    ) -> Result<bool, sqlx::Error> {
        let claimed = sqlx::query(
            "UPDATE coordination_requests SET updated_at = $1
             WHERE request_id = $2 AND state = $3 AND updated_at <= $4",
        )
        .bind(Utc::now())
        .bind(request_id)
        .bind(state.as_str())
        .bind(stale_before)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(claimed == 1)
    }

    // ── Audit log ───────────────────────────────────────────────────────────

    /// Appends an audit event that is not a state transition.
    pub async fn append_audit(
        &self,
        request_id: &str,
        event_type: &str,
        actor: &str,
        actor_type: ActorType,
        payload: Value,
    ) -> Result<AuditEvent, sqlx::Error> {
        let appended = AuditEvent {
            event_id: new_id(),
            request_id: request_id.to_string(),
            event_type: event_type.to_string(),
            actor: actor.to_string(),
            actor_type,
            payload,
            created_at: Utc::now(),
        };
        let mut tx = self.pool.begin().await?;
        self.insert_audit(&mut tx, &appended).await?;
        tx.commit().await?;
        Ok(appended)
    }

    async fn insert_audit(&self, conn: &mut PgConnection, audit: &AuditEvent) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO audit_events (event_id, request_id, event_type, actor, actor_type, payload, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&audit.event_id)
        .bind(&audit.request_id)
        .bind(&audit.event_type)
        .bind(&audit.actor)
        .bind(audit.actor_type.as_str())
        .bind(Json(&audit.payload))
        .bind(audit.created_at)
        .execute(&mut *conn)
        .await?;

        if self.outbox {
            sqlx::query("INSERT INTO vault_outbox (event_id, request_id, created_at) VALUES ($1, $2, $3)")
                .bind(&audit.event_id)
                .bind(&audit.request_id)
                .bind(audit.created_at)
                .execute(&mut *conn)
                .await?;
        }
        Ok(())
    }

    pub async fn audit_trail(&self, filter: &AuditFilter) -> Result<Vec<AuditEvent>, sqlx::Error> {
        let mut query = QueryBuilder::<Postgres>::new("SELECT e.* FROM audit_events e");
        if let Some(agent_id) = &filter.agent_id {
            query
                .push(" JOIN coordination_requests r ON r.request_id = e.request_id AND r.agent_id = ")
                .push_bind(agent_id.clone());
        }
        query.push(" WHERE TRUE");
        if let Some(request_id) = &filter.request_id {
            query.push(" AND e.request_id = ").push_bind(request_id.clone());
        }
        if let Some(event_type) = &filter.event_type {
            query.push(" AND e.event_type = ").push_bind(event_type.clone());
        }
        query.push(" ORDER BY e.created_at ASC, e.event_id ASC LIMIT ").push_bind(filter.limit);
        query.push(" OFFSET ").push_bind(filter.offset);
        query.build().fetch_all(&self.pool).await?.iter().map(audit_from_row).collect()
    }

    /// Every audit event of one request, oldest first.
    pub async fn request_events(&self, request_id: &str) -> Result<Vec<AuditEvent>, sqlx::Error> {
        sqlx::query("SELECT * FROM audit_events WHERE request_id = $1 ORDER BY created_at ASC, event_id ASC")
            .bind(request_id)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(audit_from_row)
            .collect()
    }

    pub async fn has_event(&self, request_id: &str, event_type: &str) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM audit_events WHERE request_id = $1 AND event_type = $2)")
            .bind(request_id)
            .bind(event_type)
            .fetch_one(&self.pool)
            .await
    }

    /// Channel and timestamp of the most recent Slack message posted for a request.
    pub async fn slack_message(&self, request_id: &str) -> Result<Option<(String, String)>, sqlx::Error> {
        let payload: Option<Json<Value>> = sqlx::query_scalar(
            "SELECT payload FROM audit_events WHERE request_id = $1 AND event_type = $2
             ORDER BY created_at DESC, event_id DESC LIMIT 1",
        )
        .bind(request_id)
        .bind(event::SLACK_NOTIFIED)
        .fetch_optional(&self.pool)
        .await?;
        Ok(payload.and_then(|Json(payload)| {
            let channel = payload.get("channel_id")?.as_str()?.to_string();
            let ts = payload.get("ts")?.as_str()?.to_string();
            Some((channel, ts))
        }))
    }

    // ── Vault outbox ────────────────────────────────────────────────────────

    pub async fn outbox_batch(&self, limit: i64) -> Result<Vec<OutboxEntry>, sqlx::Error> {
        let rows =
            sqlx::query("SELECT event_id, request_id FROM vault_outbox ORDER BY created_at ASC, event_id ASC LIMIT $1")
                .bind(limit)
                .fetch_all(&self.pool)
                .await?;
        rows.iter()
            .map(|row| Ok(OutboxEntry { event_id: row.try_get("event_id")?, request_id: row.try_get("request_id")? }))
            .collect()
    }

    pub async fn outbox_done(&self, event_ids: &[String]) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM vault_outbox WHERE event_id = ANY($1)")
            .bind(event_ids)
            .execute(&self.pool)
            .await
            .map(drop)
    }

    pub async fn outbox_len(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar("SELECT COUNT(*) FROM vault_outbox").fetch_one(&self.pool).await
    }

    // ── API keys ────────────────────────────────────────────────────────────

    /// Creates a key and returns it with the raw secret, which is not stored and cannot be
    /// recovered afterwards.
    pub async fn create_api_key(
        &self,
        agent_id: &str,
        label: &str,
        scopes: &[String],
    ) -> Result<(ApiKey, String), sqlx::Error> {
        let raw = generate_api_key();
        let key = ApiKey {
            key_id: new_id(),
            agent_id: agent_id.to_string(),
            label: label.to_string(),
            scopes: scopes.to_vec(),
            created_at: Utc::now(),
            revoked_at: None,
        };
        sqlx::query(
            "INSERT INTO api_keys (key_id, key_hash, agent_id, label, scopes, created_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&key.key_id)
        .bind(hash_api_key(&raw))
        .bind(&key.agent_id)
        .bind(&key.label)
        .bind(Json(&key.scopes))
        .bind(key.created_at)
        .execute(&self.pool)
        .await?;
        Ok((key, raw))
    }

    /// Looks up a live (non-revoked) key by its raw secret.
    pub async fn authenticate(&self, raw_key: &str) -> Result<Option<ApiKey>, sqlx::Error> {
        sqlx::query("SELECT * FROM api_keys WHERE key_hash = $1 AND revoked_at IS NULL")
            .bind(hash_api_key(raw_key))
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(api_key_from_row)
            .transpose()
    }

    pub async fn list_api_keys(&self) -> Result<Vec<ApiKey>, sqlx::Error> {
        sqlx::query("SELECT * FROM api_keys ORDER BY created_at ASC, key_id ASC")
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(api_key_from_row)
            .collect()
    }

    /// Returns `false` if there is no such key or it was already revoked.
    pub async fn revoke_api_key(&self, key_id: &str) -> Result<bool, sqlx::Error> {
        let revoked = sqlx::query("UPDATE api_keys SET revoked_at = $1 WHERE key_id = $2 AND revoked_at IS NULL")
            .bind(Utc::now())
            .bind(key_id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(revoked == 1)
    }
}

fn decode_error(column: &str, source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> sqlx::Error {
    sqlx::Error::ColumnDecode { index: column.to_string(), source: source.into() }
}

fn parsed<T: FromStr<Err = String>>(row: &PgRow, column: &str) -> Result<T, sqlx::Error> {
    row.try_get::<String, _>(column)?.parse().map_err(|e: String| decode_error(column, e))
}

fn request_from_row(row: &PgRow) -> Result<CoordinationRequest, sqlx::Error> {
    Ok(CoordinationRequest {
        request_id: row.try_get("request_id")?,
        agent_id: row.try_get("agent_id")?,
        intent: parsed(row, "intent")?,
        urgency: parsed(row, "urgency")?,
        state: parsed(row, "state")?,
        context_package: row.try_get::<Json<_>, _>("context_package")?.0,
        response_schema: row.try_get::<Option<Json<_>>, _>("response_schema")?.map(|json| json.0),
        timeout_policy: row.try_get::<Json<_>, _>("timeout_policy")?.0,
        routing_hints: row.try_get::<Json<_>, _>("routing_hints")?.0,
        trace_id: row.try_get("trace_id")?,
        idempotency_key: row.try_get("idempotency_key")?,
        responder_id: row.try_get("responder_id")?,
        response_data: row.try_get::<Option<Json<Value>>, _>("response_data")?.map(|json| json.0),
        responded_by: row.try_get("responded_by")?,
        responded_at: row.try_get("responded_at")?,
        submitted_at: row.try_get("submitted_at")?,
        updated_at: row.try_get("updated_at")?,
        timeout_at: row.try_get("timeout_at")?,
        delivered_at: row.try_get("delivered_at")?,
    })
}

fn audit_from_row(row: &PgRow) -> Result<AuditEvent, sqlx::Error> {
    Ok(AuditEvent {
        event_id: row.try_get("event_id")?,
        request_id: row.try_get("request_id")?,
        event_type: row.try_get("event_type")?,
        actor: row.try_get("actor")?,
        actor_type: parsed(row, "actor_type")?,
        payload: row.try_get::<Json<Value>, _>("payload")?.0,
        created_at: row.try_get("created_at")?,
    })
}

fn api_key_from_row(row: &PgRow) -> Result<ApiKey, sqlx::Error> {
    Ok(ApiKey {
        key_id: row.try_get("key_id")?,
        agent_id: row.try_get("agent_id")?,
        label: row.try_get("label")?,
        scopes: row.try_get::<Json<Vec<String>>, _>("scopes")?.0,
        created_at: row.try_get("created_at")?,
        revoked_at: row.try_get("revoked_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_hash_matches_the_original_service() {
        // sha256("hcp_test_key_1234567890"), as computed by the Node implementation.
        assert_eq!(
            hash_api_key("hcp_test_key_1234567890"),
            "e7ea4c082cc3a63ae830366f0da2c627da2d9e8649cfd10e54f079921805806a"
        );
    }

    #[test]
    fn generated_keys_are_prefixed_and_unique() {
        let (a, b) = (generate_api_key(), generate_api_key());
        assert!(a.starts_with("hcp_") && a.len() == 4 + 64);
        assert_ne!(a, b);
    }
}
