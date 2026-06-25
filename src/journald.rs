use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::{db, stream};

/// Follow systemd-journald for the given units and ingest their log lines.
/// Each unit's stdout/stderr (e.g. a Node service's console output, a Rust
/// service's tracing output) is captured by journald, so this needs no changes
/// in the apps themselves. Source is tagged with the systemd unit name.
pub fn spawn(units: Vec<String>) {
    if units.is_empty() {
        return;
    }

    tokio::spawn(async move {
        loop {
            if let Err(e) = follow(&units).await {
                tracing::warn!(error = %e, "journald follower stopped; restarting in 5s");
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}

async fn follow(units: &[String]) -> std::io::Result<()> {
    let mut cmd = Command::new("journalctl");
    cmd.args(["-f", "-o", "json", "-n", "0", "--no-pager"]);
    for u in units {
        cmd.arg("-u").arg(u);
    }
    cmd.stdout(std::process::Stdio::piped());

    let mut child = cmd.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("journalctl produced no stdout"))?;

    tracing::info!(units = units.join(","), "following journald");

    let mut lines = BufReader::new(stdout).lines();
    let mut buf: Vec<(String, String, i64)> = Vec::new();
    let mut flush = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line {
                    Ok(Some(l)) => {
                        if let Some(entry) = parse_line(&l) {
                            buf.push(entry);
                        }
                    }
                    Ok(None) => break,        // journalctl exited
                    Err(e) => return Err(e),
                }
            }
            _ = flush.tick() => {
                if !buf.is_empty() {
                    persist_and_broadcast(buf.drain(..).collect());
                }
            }
        }
    }

    if !buf.is_empty() {
        persist_and_broadcast(buf);
    }
    Ok(())
}

/// Extract (source, line, ts) from one journald JSON record.
fn parse_line(raw: &str) -> Option<(String, String, i64)> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;

    // MESSAGE is usually a string; skip binary (array) messages.
    let message = v.get("MESSAGE")?.as_str()?.to_string();
    if message.is_empty() {
        return None;
    }

    let source = v
        .get("_SYSTEMD_UNIT")
        .and_then(|s| s.as_str())
        .or_else(|| v.get("SYSLOG_IDENTIFIER").and_then(|s| s.as_str()))
        .unwrap_or("journal")
        .to_string();

    // __REALTIME_TIMESTAMP is microseconds since the epoch, as a string.
    let ts = v
        .get("__REALTIME_TIMESTAMP")
        .and_then(|s| s.as_str())
        .and_then(|s| s.parse::<i64>().ok())
        .map(|us| us / 1_000_000)
        .unwrap_or_else(|| chrono::Utc::now().timestamp());

    Some((source, message, ts))
}

fn persist_and_broadcast(entries: Vec<(String, String, i64)>) {
    db::with_db(|conn| {
        let tx = conn.unchecked_transaction().ok();
        for (source, line, ts) in &entries {
            conn.execute(
                "INSERT INTO logs (source, line, ts) VALUES (?1, ?2, ?3)",
                rusqlite::params![source, line, ts],
            )
            .ok();
        }
        if let Some(tx) = tx {
            tx.commit().ok();
        }
    });

    let logs: Vec<_> = entries
        .iter()
        .map(|(source, line, ts)| serde_json::json!({"source": source, "line": line, "ts": ts}))
        .collect();
    stream::publish("log", serde_json::json!({"logs": logs}).to_string());
}
