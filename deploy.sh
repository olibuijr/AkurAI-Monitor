#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
command -v akurai-ec2 >/dev/null || { echo "✗ akurai-ec2 not on PATH" >&2; exit 1; }
exec akurai-ec2 release "$@"
