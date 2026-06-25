mod alert;
mod auth;
mod collectors;
mod config;
mod db;
mod journald;
mod routes;
mod schema;
mod stream;
mod tailer;

use axum::{
    http::{header, HeaderValue},
    middleware,
    response::Response,
    routing::{get, post},
    Router,
};
use tower_http::services::{ServeDir, ServeFile};

// Force browsers to revalidate static assets so a redeploy is picked up
// immediately instead of serving a stale cached app.js / style.css.
async fn add_no_cache(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("rust_monitor=info")),
        )
        .init();

    tracing::info!("rust-monitor starting up");

    let cfg = config::get();

    // Initialize DB (triggers LazyLock)
    db::with_db(|_| {});

    // Initialize the live SSE broadcast channel
    stream::init();

    // Follow journald for configured systemd units (other apps log here)
    journald::spawn(cfg.journal_units.clone());

    // Spawn metric collector task.
    // Samples on a fast "live" cadence and broadcasts every snapshot to SSE
    // subscribers, but only persists to the DB on the configured interval
    // (keeping history — and chart queries — at the intended resolution).
    let interval = cfg.interval_secs;
    let live_secs = interval.min(5).max(1);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(live_secs));
        let mut last_persist: i64 = 0;
        loop {
            ticker.tick().await;
            let metrics = tokio::task::spawn_blocking(collectors::collect_all)
                .await
                .unwrap_or_default();

            let now = chrono::Utc::now().timestamp();

            // Push live snapshot to any connected dashboards
            stream::publish(
                "status",
                serde_json::json!({ "metrics": metrics, "ts": now }).to_string(),
            );

            // Persist at the configured interval only
            if now - last_persist >= interval as i64 {
                last_persist = now;
                let count = metrics.len();
                db::with_db(|conn| {
                    let tx = conn.unchecked_transaction().ok();
                    for m in &metrics {
                        conn.execute(
                            "INSERT INTO metrics (name, value, ts) VALUES (?1, ?2, ?3)",
                            rusqlite::params![m.name, m.value, now],
                        )
                        .ok();
                    }
                    if let Some(tx) = tx {
                        tx.commit().ok();
                    }
                });
                tracing::debug!(count, "collected metrics");
            }
        }
    });

    // Spawn log tailer task — tails on the fast cadence, persists every batch,
    // and broadcasts new lines live to connected log views.
    let log_files = cfg.log_files.clone();
    tokio::spawn(async move {
        let mut log_tailer = tailer::LogTailer::new(&log_files);
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(live_secs));
        loop {
            ticker.tick().await;
            let entries = log_tailer.read_new_lines();
            if entries.is_empty() {
                continue;
            }

            let now = chrono::Utc::now().timestamp();
            let count = entries.len();
            db::with_db(|conn| {
                let tx = conn.unchecked_transaction().ok();
                for (source, line) in &entries {
                    conn.execute(
                        "INSERT INTO logs (source, line, ts) VALUES (?1, ?2, ?3)",
                        rusqlite::params![source, line, now],
                    )
                    .ok();
                }
                if let Some(tx) = tx {
                    tx.commit().ok();
                }
            });

            let logs: Vec<_> = entries
                .iter()
                .map(|(source, line)| serde_json::json!({ "source": source, "line": line, "ts": now }))
                .collect();
            stream::publish("log", serde_json::json!({ "logs": logs }).to_string());

            tracing::debug!(count, "tailed log lines");
        }
    });

    // Spawn alert engine task
    let alert_log = cfg.alert_log.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            ticker.tick().await;
            alert::evaluate_alerts(&alert_log);
        }
    });

    // Spawn retention cleanup task
    let retention_days = cfg.retention_days;
    let log_retention_days = cfg.log_retention_days;
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            ticker.tick().await;
            let now = chrono::Utc::now().timestamp();
            let metric_cutoff = now - (retention_days * 86400);
            let log_cutoff = now - (log_retention_days * 86400);

            db::with_db(|conn| {
                let m = conn
                    .execute("DELETE FROM metrics WHERE ts < ?1", rusqlite::params![metric_cutoff])
                    .unwrap_or(0);
                let l = conn
                    .execute("DELETE FROM logs WHERE ts < ?1", rusqlite::params![log_cutoff])
                    .unwrap_or(0);
                if m > 0 || l > 0 {
                    tracing::info!(metrics_deleted = m, logs_deleted = l, "retention cleanup");
                }
            });
        }
    });

    // Build the router
    let index_path = format!("{}/index.html", cfg.static_dir);
    let static_service = ServeDir::new(&cfg.static_dir)
        .append_index_html_on_directories(true)
        .fallback(ServeFile::new(&index_path));

    let app = Router::new()
        .route("/api/health", get(routes::health))
        .route("/api/status", get(routes::status))
        .route("/api/metrics", get(routes::metrics))
        .route("/api/logs", get(routes::logs))
        .route("/api/alerts", get(routes::alerts))
        .route("/api/stream", get(stream::sse_handler))
        .route("/api/ingest", post(routes::ingest))
        .route("/auth/callback", get(auth::auth_callback))
        .route("/auth/logout", get(auth::auth_logout))
        .fallback_service(static_service)
        .layer(middleware::from_fn(auth::auth_middleware))
        .layer(middleware::map_response(add_no_cache));

    let listener = tokio::net::TcpListener::bind(&cfg.listen_addr).await.unwrap();
    tracing::info!("listening on {}", cfg.listen_addr);
    axum::serve(listener, app).await.unwrap();
}
