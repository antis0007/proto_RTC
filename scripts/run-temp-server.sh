#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git -C "$SCRIPT_DIR/.." rev-parse --show-toplevel 2>/dev/null || (cd "$SCRIPT_DIR/.." && pwd))"

cd "$REPO_ROOT"

mkdir -p logs
TEMP_DB="${TEMP_DB:-$(mktemp -p /tmp proto_rtc_temp_server_XXXXXX.db)}"
trap 'rm -f "$TEMP_DB"' EXIT

if [[ -f .env ]]; then
  set -a
  source .env
  set +a
fi

export SERVER_BIND="${SERVER_BIND:-0.0.0.0:8443}"
export SERVER_PUBLIC_URL="${SERVER_PUBLIC_URL:-http://127.0.0.1:8443}"
export DATABASE_URL="sqlite://$TEMP_DB"
export APP__BIND_ADDR="${APP__BIND_ADDR:-$SERVER_BIND}"
export APP__DATABASE_URL="$DATABASE_URL"
export APP__LIVEKIT_API_KEY="${APP__LIVEKIT_API_KEY:-${LIVEKIT_API_KEY:-devkey}}"
export APP__LIVEKIT_API_SECRET="${APP__LIVEKIT_API_SECRET:-${LIVEKIT_API_SECRET:-devsecret}}"

if [[ -z "${APP__LIVEKIT_URL:-}" && -n "${LIVEKIT_URL:-}" ]]; then
  export APP__LIVEKIT_URL="$LIVEKIT_URL"
fi

echo "Starting temporary server with DB: $TEMP_DB"
echo "Bind: $SERVER_BIND"
cargo run -p server -- "$@"
