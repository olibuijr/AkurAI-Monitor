#!/usr/bin/env bash
# scripts/validate.sh — Post-deploy validation for AkurAI-Monitor
set -euo pipefail

DOMAIN="monitor.olibuijr.com"
PORT=8800
RED='\033[0;31m'; GRN='\033[0;32m'; NC='\033[0m'
pass=0; fail=0
pass_() { printf "  ${GRN}PASS${NC} %s\n" "$*"; ((pass++)); }
fail_() { printf "  ${RED}FAIL${NC} %s\n" "$*"; ((fail++)); }

echo "=== Post-deploy validation: rust-monitor ==="

# 1. Systemd
systemctl is-active --quiet rust-monitor.service 2>/dev/null && pass_ "systemd active" || fail_ "systemd not active"

# 2. Loopback health
if curl -fsS --max-time 5 "http://127.0.0.1:${PORT}/api/health" > /dev/null 2>&1; then
  pass_ "loopback /api/health"
else
  fail_ "loopback /api/health"
fi

# 3. Public HTTPS
if curl -fsS --max-time 10 "https://${DOMAIN}/" > /dev/null 2>&1; then
  pass_ "public HTTPS serves page"
else
  fail_ "public HTTPS unreachable"
fi

# 4. OIDC login redirect
LOGIN_STATUS=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 10 "https://${DOMAIN}/auth/login" 2>/dev/null)
[ "$LOGIN_STATUS" = "302" ] && pass_ "/auth/login → 302 (IDP redirect)" || fail_ "/auth/login → ${LOGIN_STATUS}"

echo "━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "${GRN}Pass: $pass${NC}  ${RED}Fail: $fail${NC}"
[ "$fail" -eq 0 ] || exit 1
