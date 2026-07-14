use crate::db;
use chrono::Utc;
use serde::Serialize;
use serde_json::json;
use std::io::Write;

struct AlertRule {
    id: i64,
    host_id: Option<i64>,
    application_id: Option<i64>,
    name: String,
    metric_name: String,
    operator: String,
    threshold: f64,
    duration_secs: i64,
}

#[derive(Clone, Serialize)]
pub struct AlertNotification {
    event: &'static str,
    rule_id: i64,
    rule: String,
    metric: String,
    value: f64,
    threshold: f64,
    host_id: Option<i64>,
    application_id: Option<i64>,
    timestamp: i64,
}

struct Destination {
    channel_id: i64,
    channel_name: String,
    kind: String,
    endpoint: String,
    token_env: Option<String>,
    recipient: String,
    target: String,
}

pub async fn evaluate_alerts(alert_log_path: &str) {
    let rules = db::with_db(|conn| {
        let mut stmt = conn
            .prepare(
                "SELECT id, host_id, application_id, name, metric_name, operator, threshold, duration_secs
                 FROM alert_rules WHERE enabled = 1",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok(AlertRule {
                id: row.get(0)?,
                host_id: row.get(1)?,
                application_id: row.get(2)?,
                name: row.get(3)?,
                metric_name: row.get(4)?,
                operator: row.get(5)?,
                threshold: row.get(6)?,
                duration_secs: row.get(7)?,
            })
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect::<Vec<_>>()
    });

    let now = Utc::now().timestamp();
    let mut notifications = Vec::new();

    for rule in &rules {
        let cutoff = now - rule.duration_secs;
        let samples: Vec<f64> = db::with_db(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT value FROM metrics
                     WHERE name = ?1 AND ts >= ?2
                       AND (?3 IS NULL OR host_id = ?3)
                       AND (?4 IS NULL OR application_id = ?4)
                     ORDER BY ts",
                )
                .unwrap();
            stmt.query_map(
                rusqlite::params![rule.metric_name, cutoff, rule.host_id, rule.application_id],
                |row| row.get(0),
            )
            .unwrap()
            .filter_map(Result::ok)
            .collect()
        });

        if samples.is_empty() {
            continue;
        }

        let all_violating = samples
            .iter()
            .all(|value| violates(*value, &rule.operator, rule.threshold));
        let latest = *samples.last().unwrap();
        let has_open_alert = db::with_db(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM alert_events WHERE rule_id = ?1 AND resolved_at IS NULL",
                [rule.id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
                > 0
        });

        let event = if all_violating && !has_open_alert {
            db::with_db(|conn| {
                conn.execute(
                    "INSERT INTO alert_events (rule_id, triggered_at, value) VALUES (?1, ?2, ?3)",
                    rusqlite::params![rule.id, now, latest],
                )
                .ok();
            });
            write_alert_log(
                alert_log_path,
                &format!(
                    "ALERT rule=\"{}\" metric=\"{}\" value={:.1} threshold={:.1}",
                    rule.name, rule.metric_name, latest, rule.threshold
                ),
            );
            tracing::warn!(
                rule = rule.name,
                metric = rule.metric_name,
                value = latest,
                threshold = rule.threshold,
                "alert triggered"
            );
            Some("triggered")
        } else if !all_violating && has_open_alert {
            db::with_db(|conn| {
                conn.execute(
                    "UPDATE alert_events SET resolved_at = ?1 WHERE rule_id = ?2 AND resolved_at IS NULL",
                    rusqlite::params![now, rule.id],
                )
                .ok();
            });
            write_alert_log(
                alert_log_path,
                &format!(
                    "RESOLVED rule=\"{}\" metric=\"{}\" value={:.1}",
                    rule.name, rule.metric_name, latest
                ),
            );
            tracing::info!(
                rule = rule.name,
                metric = rule.metric_name,
                "alert resolved"
            );
            Some("resolved")
        } else {
            None
        };

        if let Some(event) = event {
            notifications.push(AlertNotification {
                event,
                rule_id: rule.id,
                rule: rule.name.clone(),
                metric: rule.metric_name.clone(),
                value: latest,
                threshold: rule.threshold,
                host_id: rule.host_id,
                application_id: rule.application_id,
                timestamp: now,
            });
        }
    }

    for notification in notifications {
        if let Err(message) = deliver(&notification, None).await {
            tracing::error!(
                error = message,
                rule = notification.rule,
                "alert delivery failed"
            );
        }
    }
}

pub async fn test_channel(channel_id: i64) -> Result<usize, String> {
    let notification = AlertNotification {
        event: "test",
        rule_id: 0,
        rule: "Channel test".to_string(),
        metric: "monitor.channel".to_string(),
        value: 1.0,
        threshold: 1.0,
        host_id: None,
        application_id: None,
        timestamp: Utc::now().timestamp(),
    };
    deliver(&notification, Some(channel_id)).await
}

async fn deliver(
    notification: &AlertNotification,
    channel_id: Option<i64>,
) -> Result<usize, String> {
    let destinations = db::with_db(|conn| {
        let mut statement = conn
            .prepare(
                "SELECT c.id, c.name, c.kind, c.endpoint, c.token_env, r.name, r.target
                 FROM recipients r
                 JOIN channels c ON c.id = r.channel_id
                 WHERE c.enabled = 1 AND r.enabled = 1 AND (?1 IS NULL OR c.id = ?1)
                 ORDER BY c.id, r.id",
            )
            .map_err(|_| "failed to query channels".to_string())?;
        statement
            .query_map([channel_id], |row| {
                Ok(Destination {
                    channel_id: row.get(0)?,
                    channel_name: row.get(1)?,
                    kind: row.get(2)?,
                    endpoint: row.get(3)?,
                    token_env: row.get(4)?,
                    recipient: row.get(5)?,
                    target: row.get(6)?,
                })
            })
            .map_err(|_| "failed to query recipients".to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|_| "failed to read recipients".to_string())
    })?;

    if channel_id.is_some() && destinations.is_empty() {
        return Err("channel has no enabled recipients".to_string());
    }

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|_| "failed to create alert HTTP client".to_string())?;
    let mut sent = 0;
    let mut failures = Vec::new();
    for destination in destinations {
        match deliver_one(&client, notification, &destination).await {
            Ok(()) => sent += 1,
            Err(message) => failures.push(format!(
                "channel {} ({}) recipient {}: {message}",
                destination.channel_id, destination.channel_name, destination.recipient
            )),
        }
    }

    if failures.is_empty() {
        Ok(sent)
    } else {
        Err(failures.join("; "))
    }
}

async fn deliver_one(
    client: &reqwest::Client,
    notification: &AlertNotification,
    destination: &Destination,
) -> Result<(), String> {
    match destination.kind.as_str() {
        "webhook" => {
            let webhook_client = webhook_client(&destination.endpoint).await?;
            let mut request = webhook_client.post(&destination.endpoint).json(&json!({
                "recipient": {
                    "name": destination.recipient,
                    "target": destination.target,
                },
                "alert": notification,
            }));
            if let Some(token) = channel_token(destination)? {
                request = request.bearer_auth(token);
            }
            let response = request.send().await.map_err(|error| error.to_string())?;
            if response.status().is_success() {
                Ok(())
            } else {
                Err(format!("webhook returned HTTP {}", response.status()))
            }
        }
        "pi-bun" => deliver_pi_bun(client, notification, destination).await,
        _ => Err("unsupported channel kind".to_string()),
    }
}

async fn deliver_pi_bun(
    client: &reqwest::Client,
    notification: &AlertNotification,
    destination: &Destination,
) -> Result<(), String> {
    let endpoint = reqwest::Url::parse(&destination.endpoint)
        .map_err(|_| "pi-bun endpoint is invalid".to_string())?;
    if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str() != Some("100.88.0.9") {
        return Err("pi-bun endpoint is outside the Titan mesh".to_string());
    }
    let token = channel_token(destination)?
        .ok_or_else(|| "pi-bun channel requires token_env".to_string())?;
    let base = destination.endpoint.trim_end_matches('/');
    let login = client
        .post(format!("{base}/api/login"))
        .json(&json!({"token": token}))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !login.status().is_success() {
        return Err(format!("pi-bun login returned HTTP {}", login.status()));
    }
    let cookie = login
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .ok_or_else(|| "pi-bun login returned no session cookie".to_string())?;
    let session = percent_encode_path(&destination.target);
    let message = format!(
        "AkurAI Monitor {} alert: {} ({} = {:.2}, threshold {:.2}). Investigate with akurai-monitorctl.",
        notification.event,
        notification.rule,
        notification.metric,
        notification.value,
        notification.threshold
    );
    let response = client
        .post(format!("{base}/api/sessions/{session}/command"))
        .header(reqwest::header::COOKIE, cookie)
        .json(&json!({"command": {"type": "prompt", "message": message}}))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!(
            "pi-bun command returned HTTP {}",
            response.status()
        ))
    }
}

fn channel_token(destination: &Destination) -> Result<Option<String>, String> {
    destination
        .token_env
        .as_deref()
        .map(|name| std::env::var(name).map_err(|_| "channel token is unavailable".to_string()))
        .transpose()
}

async fn webhook_client(endpoint: &str) -> Result<reqwest::Client, String> {
    let url =
        reqwest::Url::parse(endpoint).map_err(|_| "webhook endpoint is invalid".to_string())?;
    let host = url
        .host_str()
        .ok_or_else(|| "webhook endpoint has no host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "webhook endpoint has no port".to_string())?;
    let addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| "webhook endpoint could not be resolved".to_string())?
        .collect::<Vec<_>>();
    if addresses.is_empty() || addresses.iter().any(|address| !public_ip(address.ip())) {
        return Err("webhook endpoint resolves to a non-public address".to_string());
    }
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15))
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|_| "failed to create webhook HTTP client".to_string())
}

fn public_ip(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(address) => {
            let octets = address.octets();
            !(address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_unspecified()
                || address.is_broadcast()
                || address.is_multicast()
                || address.is_documentation()
                || octets[0] == 0
                || octets[0] >= 240
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 198 && matches!(octets[1], 18 | 19)))
        }
        std::net::IpAddr::V6(address) => {
            if let Some(address) = address.to_ipv4_mapped() {
                return public_ip(std::net::IpAddr::V4(address));
            }
            !(address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_unique_local()
                || address.is_unicast_link_local())
        }
    }
}

fn percent_encode_path(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn violates(value: f64, operator: &str, threshold: f64) -> bool {
    match operator {
        "gt" => value > threshold,
        "lt" => value < threshold,
        "eq" => (value - threshold).abs() < f64::EPSILON,
        _ => false,
    }
}

fn write_alert_log(path: &str, message: &str) {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let timestamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{timestamp} {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn webhook_addresses_must_be_public() {
        assert!(public_ip(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(public_ip(IpAddr::V6(
            "2606:4700:4700::1111".parse::<Ipv6Addr>().unwrap()
        )));
        for address in [
            Ipv4Addr::new(127, 0, 0, 1),
            Ipv4Addr::new(10, 0, 0, 1),
            Ipv4Addr::new(100, 88, 0, 9),
            Ipv4Addr::new(169, 254, 169, 254),
            Ipv4Addr::new(192, 0, 2, 1),
        ] {
            assert!(!public_ip(IpAddr::V4(address)));
        }
    }
}
