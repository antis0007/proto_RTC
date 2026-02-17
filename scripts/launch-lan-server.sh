#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "Usage: $0 <LAN_IP_OR_HOSTNAME> [PORT] [BIND_IP] [extra server args...]" >&2
  echo "Example: $0 192.168.1.42 8443 0.0.0.0" >&2
  exit 1
fi

LAN_HOST="$1"
shift || true
PORT="${1:-8443}"
if [[ $# -gt 0 ]]; then
  shift || true
fi

BIND_IP="${1:-0.0.0.0}"
if [[ $# -gt 0 ]]; then
  shift || true
fi

export SERVER_BIND="${BIND_IP}:${PORT}"
export APP__BIND_ADDR="$SERVER_BIND"
export SERVER_PUBLIC_URL="http://${LAN_HOST}:${PORT}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
"$SCRIPT_DIR/run-server.sh" "$@"
