# CLAUDE.md

## What this is

Minimalistic Zabbix-style monitoring for a single Linux host. Collects system metrics from /proc, tails log files, evaluates alert rules, writes alerts to a log file, and serves a web dashboard. Runs on AWS EC2 (Ubuntu 22.04).

## Build & Deploy

- `cargo build --release --target x86_64-unknown-linux-musl` (static binary)
- `./deploy.sh` — builds, uploads to VM, restarts service, healthchecks
- VM: `ssh akurai-mail` (3.94.46.219, Ubuntu 22.04, systemd `rust-monitor.service`)

## Architecture

```
nginx (TLS) → rust-monitor (:8800)
              ├─ collectors.rs  → reads /proc, inserts metrics into SQLite
              ├─ tailer.rs      → tails log files, inserts into SQLite
              ├─ alert.rs       → evaluates rules, writes alerts.log
              ├─ routes.rs      → API endpoints for dashboard
              └─ ui/            → static HTML/CSS/JS dashboard
```

## Source Layout

- `src/main.rs` — tracing init, config, spawn background tasks, axum server
- `src/config.rs` — env var config (LazyLock)
- `src/db.rs` — SQLite init + with_db() accessor
- `src/schema.rs` — CREATE TABLE statements + seed alert rules
- `src/collectors.rs` — CPU/mem/disk/net/load/uptime from /proc
- `src/tailer.rs` — log file tailing (seek-to-end, poll new lines)
- `src/alert.rs` — alert rule evaluation + log file writer
- `src/routes.rs` — API: /api/health, /api/status, /api/metrics, /api/logs, /api/alerts
- `src/auth.rs` — OIDC/PKCE login via auth.olibuijr.com, in-memory sessions, axum middleware

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `MONITOR_LISTEN` | `127.0.0.1:8800` | Bind address |
| `MONITOR_DB_PATH` | `./data/monitor.db` | SQLite path |
| `MONITOR_STATIC_DIR` | `./ui` | Static UI files |
| `MONITOR_ALERT_LOG` | `./data/alerts.log` | Alert output file |
| `MONITOR_LOG_FILES` | `/var/log/syslog,/var/log/auth.log` | Log files to tail |
| `MONITOR_INTERVAL` | `60` | Collection interval (seconds) |
| `MONITOR_RETENTION_DAYS` | `30` | Metric retention |
| `MONITOR_LOG_RETENTION_DAYS` | `7` | Log retention |
| `MONITOR_JOURNAL_UNITS` | (empty) | Comma-separated systemd units to follow via journald (same-host log collection) |
| `MONITOR_INGEST_TOKEN` | (empty) | Bearer token for `POST /api/ingest`; empty disables ingestion |
| `MONITOR_OIDC_ISSUER` / `_CLIENT_ID` / `_CLIENT_SECRET` | — | OIDC SSO config |

## Collecting logs from other apps

Two paths, no per-app code required:

**Same-host (preferred) — journald.** Other systemd services on this host log
their stdout/stderr to journald. Set `MONITOR_JOURNAL_UNITS=akurai-idp.service,…`
and the monitor follows `journalctl -f -o json` for those units, tagging each
log line's source with the unit name. Zero changes in the apps. Requires read
access to the journal (the service runs as root). See `src/journald.rs`.

**Cross-host — HTTP ingest.** Remote apps POST to `/api/ingest` (token-authed,
bypasses OIDC):

```
POST /api/ingest
Authorization: Bearer $MONITOR_INGEST_TOKEN
{ "logs": [ { "source": "localai", "line": "INFO request ok …", "ts": 1750000000 } ] }
```

`ts` is optional (defaults to now). Lines are inserted and broadcast live. A
remote box can ship with `journalctl -f -o json -u <unit> | <poster>` that POSTs
batches to `/api/ingest` with the bearer token.

## Real-time

- `/api/stream` is an SSE endpoint; the UI subscribes and updates live, falling
  back to polling if the stream can't open. Collector samples ~5s for live push
  but persists at `MONITOR_INTERVAL`.

## Constraints

- Binary MUST cross-compile with `x86_64-unknown-linux-musl`
- Release profile: opt-level=z, LTO, strip, panic=abort
- Auth: OIDC/PKCE via auth.olibuijr.com (see src/auth.rs); behind nginx + TLS
- Static UI: plain HTML + vanilla JS, no build step (charts are hand-rolled canvas, no chart lib)
