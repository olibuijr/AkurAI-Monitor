#!/usr/bin/env bash
# Deploy rules of conduct:
# - Read AGENTS.md and canonical Notes/docs before changing or deploying.
# - Never print, commit, or copy secrets; use passvault/env files only.
# - Preserve env files, service users, user data, and databases; snapshot state before risky swaps.
# - Verify ports, DNS, TLS, and systemd unit names before changing routes or services.
# - Use managed services only (systemd/pm2); no nohup, background shells, or ad hoc daemons.
# - Run gates and health checks; if deploy fails, stop and roll back rather than improvising.
# - Keep deploy behavior unchanged unless the task explicitly asks for deploy logic changes.
set -euo pipefail

SSH_HOST="${MONITOR_SSH:-akurai-mail}"
DEPLOY_DIR="/opt/rust-monitor"
SERVICE="rust-monitor"

log()  { echo "[deploy] $*"; }
step() { echo; echo "── $* ──"; }

# ---------------------------------------------------------------------------
# Step 1: Build Rust binary (release, musl)
# ---------------------------------------------------------------------------
step "1/4  Build Rust binary"
TARGET="x86_64-unknown-linux-musl"
CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --target "$TARGET" 2>&1
BINARY="target/$TARGET/release/rust-monitor"
log "Binary: $(du -sh "$BINARY" | cut -f1)"

# ---------------------------------------------------------------------------
# Step 2: Upload binary + UI files
# ---------------------------------------------------------------------------
step "2/4  Upload"
ssh "$SSH_HOST" "sudo mkdir -p $DEPLOY_DIR/ui /var/lib/rust-monitor /var/log/rust-monitor"
scp -q "$BINARY" "$SSH_HOST:/tmp/rust-monitor"
ssh "$SSH_HOST" "sudo install -m 755 /tmp/rust-monitor $DEPLOY_DIR/rust-monitor && rm /tmp/rust-monitor"
rsync -az --delete ui/ "$SSH_HOST:/tmp/rust-monitor-ui/"
ssh "$SSH_HOST" "sudo rsync -a --delete /tmp/rust-monitor-ui/ $DEPLOY_DIR/ui/ && sudo rm -rf /tmp/rust-monitor-ui"
log "Uploaded binary + static files"

# ---------------------------------------------------------------------------
# Step 3: Install systemd service + restart
# ---------------------------------------------------------------------------
step "3/4  Install service"
ssh "$SSH_HOST" "sudo bash -s" <<'INSTALL'
set -euo pipefail

# Environment file (only create if missing)
ENV_FILE="/etc/rust-monitor.env"
if [ ! -f "$ENV_FILE" ]; then
    cat > "$ENV_FILE" <<EOF
MONITOR_LISTEN=127.0.0.1:8800
MONITOR_DB_PATH=/var/lib/rust-monitor/monitor.db
MONITOR_STATIC_DIR=/opt/rust-monitor/ui
MONITOR_ALERT_LOG=/var/log/rust-monitor/alerts.log
MONITOR_LOG_FILES=/var/log/syslog,/var/log/auth.log,/var/log/nginx/access.log,/var/log/nginx/error.log,/var/log/mail.log
MONITOR_INTERVAL=60
MONITOR_RETENTION_DAYS=30
MONITOR_LOG_RETENTION_DAYS=7
RUST_LOG=rust_monitor=info
EOF
    chmod 600 "$ENV_FILE"
    echo "Created $ENV_FILE"
fi

# Systemd unit
cat > /etc/systemd/system/rust-monitor.service <<UNIT
[Unit]
Description=rust-monitor — system monitoring
After=network.target

[Service]
Type=simple
ExecStart=/opt/rust-monitor/rust-monitor
WorkingDirectory=/opt/rust-monitor
EnvironmentFile=/etc/rust-monitor.env
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable rust-monitor
systemctl restart rust-monitor
echo "Service restarted"
INSTALL

# ---------------------------------------------------------------------------
# Step 4: Healthcheck
# ---------------------------------------------------------------------------
step "4/4  Healthcheck"
sleep 2
ssh "$SSH_HOST" "curl -fsS http://127.0.0.1:8800/api/health"
echo
log "Deploy complete"
