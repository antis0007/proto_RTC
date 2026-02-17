#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git -C "$SCRIPT_DIR/.." rev-parse --show-toplevel 2>/dev/null || (cd "$SCRIPT_DIR/.." && pwd))"

cd "$REPO_ROOT"

mkdir -p data logs

if [[ -f .env ]]; then
  set -a
  source .env
  set +a
fi

export SERVER_BIND="${SERVER_BIND:-127.0.0.1:8443}"
export DATABASE_URL="${DATABASE_URL:-sqlite://./data/server.db}"
export APP__BIND_ADDR="$SERVER_BIND"
export APP__DATABASE_URL="${APP__DATABASE_URL:-$DATABASE_URL}"
export APP__LIVEKIT_API_KEY="${APP__LIVEKIT_API_KEY:-${LIVEKIT_API_KEY:-devkey}}"
export APP__LIVEKIT_API_SECRET="${APP__LIVEKIT_API_SECRET:-${LIVEKIT_API_SECRET:-devsecret}}"
export APP__LIVEKIT_URL="${APP__LIVEKIT_URL:-${LIVEKIT_URL:-}}"

cargo run -p server -- "$@"
