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

export SERVER_PUBLIC_URL="${SERVER_PUBLIC_URL:-http://127.0.0.1:8443}"

if [[ -f apps/desktop_gui/Cargo.toml ]]; then
  cargo run -p desktop_gui -- "$@"
else
  echo "apps/desktop_gui not found; falling back to apps/desktop." >&2
  cargo run -p desktop -- --server-url "$SERVER_PUBLIC_URL" --username "${CLI_USERNAME:-gui-user}" "$@"
fi
