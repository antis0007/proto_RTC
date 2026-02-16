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
export CLI_USERNAME="${CLI_USERNAME:-local-user}"

cargo run -p desktop -- --server-url "$SERVER_PUBLIC_URL" --username "$CLI_USERNAME" "$@"
