use axum::extract::Query;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{config, db, stream};

pub async fn health() -> impl IntoResponse {
    Json(json!({"app": "akurai-monitor", "status": "ok", "version": env!("CARGO_PKG_VERSION")}))
}

#[derive(Serialize)]
struct MetricPoint {
    name: String,
    value: f64,
    ts: i64,
}

pub async fn status() -> impl IntoResponse {
    // Latest value of each metric
    let metrics = db::with_db(|conn| {
        let mut stmt = conn
            .prepare(
                "SELECT name, value, ts FROM metrics WHERE id IN (SELECT MAX(id) FROM metrics GROUP BY name) ORDER BY name",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok(MetricPoint {
                name: row.get(0)?,
                value: row.get(1)?,
                ts: row.get(2)?,
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
}

pub async fn metrics(Query(q): Query<MetricsQuery>) -> impl IntoResponse {
    let hours = q.hours.unwrap_or(24);
    let cutoff = chrono::Utc::now().timestamp() - (hours * 3600);

    let metrics = db::with_db(|conn| {
        if let Some(name) = &q.name {
            let mut stmt = conn
                .prepare("SELECT name, value, ts FROM metrics WHERE name = ?1 AND ts >= ?2 ORDER BY ts")
                .unwrap();
            stmt.query_map(rusqlite::params![name, cutoff], |row| {
                Ok(MetricPoint {
                    name: row.get(0)?,
                    value: row.get(1)?,
                    ts: row.get(2)?,
                })
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>()
        } else {
            let mut stmt = conn
                .prepare("SELECT name, value, ts FROM metrics WHERE ts >= ?1 ORDER BY ts LIMIT 1000")
                .unwrap();
            stmt.query_map(rusqlite::params![cutoff], |row| {
                Ok(MetricPoint {
                    name: row.get(0)?,
                    value: row.get(1)?,
                    ts: row.get(2)?,
                })
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>()
        }
    });

    Json(json!({"metrics": metrics, "count": metrics.len()}))
}

#[derive(Deserialize)]
pub struct LogsQuery {
    pub source: Option<String>,
    pub hours: Option<i64>,
}

#[derive(Serialize)]
struct LogEntry {
    source: String,
    line: String,
    ts: i64,
}

pub async fn logs(Query(q): Query<LogsQuery>) -> impl IntoResponse {
    let hours = q.hours.unwrap_or(1);
    let cutoff = chrono::Utc::now().timestamp() - (hours * 3600);

    let entries = db::with_db(|conn| {
        if let Some(source) = &q.source {
            let mut stmt = conn
                .prepare("SELECT source, line, ts FROM logs WHERE source = ?1 AND ts >= ?2 ORDER BY ts DESC LIMIT 500")
                .unwrap();
            stmt.query_map(rusqlite::params![source, cutoff], |row| {
                Ok(LogEntry {
                    source: row.get(0)?,
                    line: row.get(1)?,
                    ts: row.get(2)?,
                })
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>()
        } else {
            let mut stmt = conn
                .prepare("SELECT source, line, ts FROM logs WHERE ts >= ?1 ORDER BY ts DESC LIMIT 500")
                .unwrap();
            stmt.query_map(rusqlite::params![cutoff], |row| {
                Ok(LogEntry {
                    source: row.get(0)?,
                    line: row.get(1)?,
                    ts: row.get(2)?,
                })
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>()
        }
    });

    Json(json!({"logs": entries, "count": entries.len()}))
}

#[derive(Deserialize)]
pub struct AlertsQuery {
    pub hours: Option<i64>,
}

#[derive(Serialize)]
struct AlertEvent {
    id: i64,
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
                "SELECT e.id, r.name, r.metric_name, r.threshold, e.value, e.triggered_at, e.resolved_at
                 FROM alert_events e JOIN alert_rules r ON e.rule_id = r.id
                 WHERE e.triggered_at >= ?1 OR e.resolved_at IS NULL
                 ORDER BY e.triggered_at DESC",
            )
            .unwrap();
        stmt.query_map(rusqlite::params![cutoff], |row| {
            Ok(AlertEvent {
                id: row.get(0)?,
                rule_name: row.get(1)?,
                metric_name: row.get(2)?,
                threshold: row.get(3)?,
                value: row.get(4)?,
                triggered_at: row.get(5)?,
                resolved_at: row.get(6)?,
            })
        })
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

    let now = chrono::Utc::now().timestamp();
    let inserted = db::with_db(|conn| {
        let tx = conn.unchecked_transaction().ok();
        let mut n = 0i64;
        for l in &body.logs {
            let ts = l.ts.unwrap_or(now);
            if conn
                .execute(
                    "INSERT INTO logs (source, line, ts) VALUES (?1, ?2, ?3)",
                    rusqlite::params![l.source, l.line, ts],
                )
                .is_ok()
            {
                n += 1;
            }
        }
        if let Some(tx) = tx {
            tx.commit().ok();
        }
        n
    });

    // Broadcast to live log views so shipped logs appear in real time
    let logs: Vec<_> = body
        .logs
        .iter()
        .map(|l| json!({"source": l.source, "line": l.line, "ts": l.ts.unwrap_or(now)}))
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

