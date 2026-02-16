#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "Usage: $0 <SERVER_URL> [USERNAME] [extra client args...]" >&2
  echo "Example: $0 http://192.168.1.42:8443 alice" >&2
  exit 1
fi

SERVER_URL="$1"
shift || true
USERNAME="${1:-remote-user}"
if [[ $# -gt 0 ]]; then
  shift || true
fi

export SERVER_PUBLIC_URL="$SERVER_URL"
export CLI_USERNAME="$USERNAME"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
"$SCRIPT_DIR/run-cli.sh" "$@"
