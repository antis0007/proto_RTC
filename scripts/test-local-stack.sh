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
export SERVER_PUBLIC_URL="${SERVER_PUBLIC_URL:-http://127.0.0.1:8443}"
export DATABASE_URL="${DATABASE_URL:-sqlite://./data/server.db}"
export APP__BIND_ADDR="$SERVER_BIND"
export APP__DATABASE_URL="${APP__DATABASE_URL:-$DATABASE_URL}"
export APP__LIVEKIT_API_KEY="${APP__LIVEKIT_API_KEY:-${LIVEKIT_API_KEY:-devkey}}"
export APP__LIVEKIT_API_SECRET="${APP__LIVEKIT_API_SECRET:-${LIVEKIT_API_SECRET:-devsecret}}"
if [[ -z "${APP__LIVEKIT_URL:-}" && -n "${LIVEKIT_URL:-}" ]]; then
  export APP__LIVEKIT_URL="$LIVEKIT_URL"
fi

SERVER_LOG="logs/test-local-server.log"
CLIENT1_LOG="logs/test-local-client-1.log"
CLIENT2_LOG="logs/test-local-client-2.log"

cleanup() {
  if [[ -n "${SERVER_PID:-}" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

cargo run -p server >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!

echo "Started server (pid=$SERVER_PID). Waiting for /healthz on $SERVER_PUBLIC_URL ..."
for _ in {1..30}; do
  if curl --silent --show-error --fail "$SERVER_PUBLIC_URL/healthz" >/dev/null; then
    echo "Server is healthy."
    break
  fi
  sleep 1
done

if ! curl --silent --show-error --fail "$SERVER_PUBLIC_URL/healthz" >/dev/null; then
  echo "Server never became healthy. See $SERVER_LOG" >&2
  exit 1
fi

cargo run -p desktop -- --server-url "$SERVER_PUBLIC_URL" --username "${CLIENT1_USERNAME:-local-user-1}" >"$CLIENT1_LOG" 2>&1
cargo run -p desktop -- --server-url "$SERVER_PUBLIC_URL" --username "${CLIENT2_USERNAME:-local-user-2}" >"$CLIENT2_LOG" 2>&1

echo "Local stack smoke test passed. Logs:"
echo "  $SERVER_LOG"
echo "  $CLIENT1_LOG"
echo "  $CLIENT2_LOG"
