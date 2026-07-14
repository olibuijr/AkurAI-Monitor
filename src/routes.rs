use axum::Json;
use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{config, db, stream};

pub async fn health() -> impl IntoResponse {
    Json(json!({"app": "akurai-monitor", "status": "ok", "version": env!("CARGO_PKG_VERSION")}))
}

#[derive(Serialize)]
struct MetricPoint {
    host_id: Option<i64>,
    application_id: Option<i64>,
    name: String,
    value: f64,
    ts: i64,
}

pub async fn status() -> impl IntoResponse {
    // Latest value of each metric
    let metrics = db::with_db(|conn| {
        let mut stmt = conn
            .prepare(
                "SELECT host_id, application_id, name, value, ts
                 FROM metrics
                 WHERE id IN (
                    SELECT MAX(id) FROM metrics GROUP BY host_id, application_id, name
                 )
                 ORDER BY host_id, application_id, name",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok(MetricPoint {
                host_id: row.get(0)?,
                application_id: row.get(1)?,
                name: row.get(2)?,
                value: row.get(3)?,
                ts: row.get(4)?,
            })
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect::<Vec<_>>()
    });

    Json(json!({"metrics": metrics}))
}

#[derive(Deserialize)]
pub struct MetricsQuery {
    pub name: Option<String>,
    pub hours: Option<i64>,
    pub host_id: Option<i64>,
    pub application_id: Option<i64>,
}

pub async fn metrics(Query(q): Query<MetricsQuery>) -> impl IntoResponse {
    let hours = q.hours.unwrap_or(24);
    let cutoff = chrono::Utc::now().timestamp() - (hours * 3600);
    let metrics = db::with_db(|conn| {
        let mut stmt = conn
            .prepare(
                "SELECT host_id, application_id, name, value, ts
                 FROM metrics
                 WHERE ts >= ?1
                   AND (?2 IS NULL OR name = ?2)
                   AND (?3 IS NULL OR host_id = ?3)
                   AND (?4 IS NULL OR application_id = ?4)
                 ORDER BY ts
                 LIMIT 1000",
            )
            .unwrap();
        stmt.query_map(
            rusqlite::params![cutoff, q.name, q.host_id, q.application_id],
            |row| {
                Ok(MetricPoint {
                    host_id: row.get(0)?,
                    application_id: row.get(1)?,
                    name: row.get(2)?,
                    value: row.get(3)?,
                    ts: row.get(4)?,
                })
            },
        )
        .unwrap()
        .filter_map(Result::ok)
        .collect::<Vec<_>>()
    });

    Json(json!({"metrics": metrics, "count": metrics.len()}))
}

#[derive(Deserialize)]
pub struct LogsQuery {
    pub source: Option<String>,
    pub hours: Option<i64>,
    pub host_id: Option<i64>,
    pub application_id: Option<i64>,
}

#[derive(Serialize)]
struct LogEntry {
    host_id: Option<i64>,
    application_id: Option<i64>,
    source: String,
    line: String,
    ts: i64,
}

pub async fn logs(Query(q): Query<LogsQuery>) -> impl IntoResponse {
    let hours = q.hours.unwrap_or(1);
    let cutoff = chrono::Utc::now().timestamp() - (hours * 3600);
    let entries = db::with_db(|conn| {
        let mut stmt = conn
            .prepare(
                "SELECT host_id, application_id, source, line, ts
                 FROM logs
                 WHERE ts >= ?1
                   AND (?2 IS NULL OR source = ?2)
                   AND (?3 IS NULL OR host_id = ?3)
                   AND (?4 IS NULL OR application_id = ?4)
                 ORDER BY ts DESC
                 LIMIT 500",
            )
            .unwrap();
        stmt.query_map(
            rusqlite::params![cutoff, q.source, q.host_id, q.application_id],
            |row| {
                Ok(LogEntry {
                    host_id: row.get(0)?,
                    application_id: row.get(1)?,
                    source: row.get(2)?,
                    line: row.get(3)?,
                    ts: row.get(4)?,
                })
            },
        )
        .unwrap()
        .filter_map(Result::ok)
        .collect::<Vec<_>>()
    });

    Json(json!({"logs": entries, "count": entries.len()}))
}

#[derive(Deserialize)]
pub struct AlertsQuery {
    pub hours: Option<i64>,
    pub host_id: Option<i64>,
    pub application_id: Option<i64>,
}

#[derive(Serialize)]
struct AlertEvent {
    id: i64,
    host_id: Option<i64>,
    application_id: Option<i64>,
    rule_name: String,
    metric_name: String,
    threshold: f64,
    value: f64,
    triggered_at: i64,
    resolved_at: Option<i64>,
}

pub async fn alerts(Query(q): Query<AlertsQuery>) -> impl IntoResponse {
    let hours = q.hours.unwrap_or(24);
    let cutoff = chrono::Utc::now().timestamp() - (hours * 3600);

    let events = db::with_db(|conn| {
        let mut stmt = conn
            .prepare(
                "SELECT e.id, r.host_id, r.application_id, r.name, r.metric_name,
                        r.threshold, e.value, e.triggered_at, e.resolved_at
                 FROM alert_events e JOIN alert_rules r ON e.rule_id = r.id
                 WHERE (e.triggered_at >= ?1 OR e.resolved_at IS NULL)
                   AND (?2 IS NULL OR r.host_id = ?2)
                   AND (?3 IS NULL OR r.application_id = ?3)
                 ORDER BY e.triggered_at DESC",
            )
            .unwrap();
        stmt.query_map(
            rusqlite::params![cutoff, q.host_id, q.application_id],
            |row| {
                Ok(AlertEvent {
                    id: row.get(0)?,
                    host_id: row.get(1)?,
                    application_id: row.get(2)?,
                    rule_name: row.get(3)?,
                    metric_name: row.get(4)?,
                    threshold: row.get(5)?,
                    value: row.get(6)?,
                    triggered_at: row.get(7)?,
                    resolved_at: row.get(8)?,
                })
            },
        )
        .unwrap()
        .filter_map(|r| r.ok())
        .collect::<Vec<_>>()
    });

    let active = events.iter().filter(|e| e.resolved_at.is_none()).count();
    Json(json!({"alerts": events, "active": active, "total": events.len()}))
}

#[derive(Deserialize)]
pub struct IngestLine {
    pub source: String,
    pub line: String,
    pub ts: Option<i64>,
    pub host_id: Option<i64>,
    pub application_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct IngestBody {
    pub logs: Vec<IngestLine>,
}

/// POST /api/ingest — accept log lines shipped from other applications.
/// Authenticated by a bearer token (MONITOR_INGEST_TOKEN), not OIDC.
pub async fn ingest(headers: HeaderMap, Json(body): Json<IngestBody>) -> impl IntoResponse {
    let token = config::get().ingest_token.as_str();
    if token.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "ingest disabled").into_response();
    }

    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-ingest-token").and_then(|v| v.to_str().ok()));

    // Length-aware constant-ish comparison
    if provided.map(|p| p.as_bytes().ct_eq(token.as_bytes())) != Some(true) {
        return (StatusCode::UNAUTHORIZED, "invalid ingest token").into_response();
    }

    if body.logs.is_empty() {
        return Json(json!({"inserted": 0})).into_response();
    }

    let invalid_ownership = db::with_db(|conn| {
        body.logs.iter().any(|line| {
            let Some(application_id) = line.application_id else {
                return false;
            };
            let Some(host_id) = line.host_id else {
                return true;
            };
            conn.query_row(
                "SELECT COUNT(*) FROM applications WHERE id = ?1 AND host_id = ?2",
                rusqlite::params![application_id, host_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
                == 0
        })
    });
    if invalid_ownership {
        return (
            StatusCode::BAD_REQUEST,
            "application does not belong to host",
        )
            .into_response();
    }

    let now = chrono::Utc::now().timestamp();
    let inserted = db::with_db(|conn| {
        let tx = conn.unchecked_transaction()?;
        let mut inserted = 0i64;
        for line in &body.logs {
            let ts = line.ts.unwrap_or(now);
            tx.execute(
                "INSERT INTO logs (host_id, application_id, source, line, ts)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    line.host_id,
                    line.application_id,
                    line.source,
                    line.line,
                    ts
                ],
            )?;
            inserted += 1;
        }
        tx.commit()?;
        Ok::<_, rusqlite::Error>(inserted)
    });
    let Ok(inserted) = inserted else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "failed to store logs").into_response();
    };

    // Broadcast to live log views so shipped logs appear in real time
    let logs: Vec<_> = body
        .logs
        .iter()
        .map(|l| {
            json!({
                "host_id": l.host_id,
                "application_id": l.application_id,
                "source": l.source,
                "line": l.line,
                "ts": l.ts.unwrap_or(now)
            })
        })
        .collect();
    stream::publish("log", json!({"logs": logs}).to_string());

    Json(json!({"inserted": inserted})).into_response()
}

/// Tiny constant-time byte comparison to avoid token timing leaks.
trait CtEq {
    fn ct_eq(&self, other: &[u8]) -> bool;
}
impl CtEq for [u8] {
    fn ct_eq(&self, other: &[u8]) -> bool {
        if self.len() != other.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in self.iter().zip(other.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}
