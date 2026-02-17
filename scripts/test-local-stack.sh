#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git -C "$SCRIPT_DIR/.." rev-parse --show-toplevel 2>/dev/null || (cd "$SCRIPT_DIR/.." && pwd))"

cd "$REPO_ROOT"

mkdir -p data logs artifacts/local-stack

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
ARTIFACT_DIR="artifacts/local-stack"
RUN_SUMMARY="$ARTIFACT_DIR/run-summary.log"

rm -f "$ARTIFACT_DIR"/*
: >"$SERVER_LOG"
: >"$CLIENT1_LOG"
: >"$CLIENT2_LOG"
: >"$RUN_SUMMARY"

echo "[client1] local stack smoke run" >>"$CLIENT1_LOG"
echo "[client2] local stack smoke run" >>"$CLIENT2_LOG"
echo "[run] local stack smoke run" >>"$RUN_SUMMARY"

mark_ok() {
  local step="$1"; shift
  echo "[OK] $step" | tee -a "$RUN_SUMMARY"
}

cleanup() {
  local status=$?
  if [[ -n "${SERVER_PID:-}" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi

  if [[ $status -eq 0 ]]; then
    echo "PASS: local stack smoke gate completed."
  else
    echo "FAIL: local stack smoke gate failed." >&2
  fi

  echo "Artifacts collected:" 
  echo "  $SERVER_LOG"
  echo "  $CLIENT1_LOG"
  echo "  $CLIENT2_LOG"
  echo "  $ARTIFACT_DIR"
  exit $status
}
trap cleanup EXIT

request() {
  local method="$1"; shift
  local url="$1"; shift
  local expected_code="$1"; shift
  local out_file="$1"; shift

  local http_code
  http_code=$(curl -sS -o "$out_file" -w "%{http_code}" -X "$method" "$url" "$@")
  if [[ "$http_code" != "$expected_code" ]]; then
    echo "Request failed: $method $url (expected $expected_code got $http_code). Body: $out_file" >&2
    return 1
  fi
}

extract_json() {
  local file="$1"; shift
  local expr="$1"; shift
  python3 - "$file" "$expr" <<'PY'
import json
import sys

file_path = sys.argv[1]
expr = sys.argv[2]
with open(file_path, "r", encoding="utf-8") as f:
    data = json.load(f)
cur = data
for part in expr.split('.'):
    if part.isdigit():
        cur = cur[int(part)]
    else:
        cur = cur[part]
if isinstance(cur, (dict, list)):
    print(json.dumps(cur))
else:
    print(cur)
PY
}

contains_message() {
  local file="$1"; shift
  local sender_id="$1"; shift
  local ciphertext="$1"; shift
  python3 - "$file" "$sender_id" "$ciphertext" <<'PY'
import json
import sys

file_path, sender_id, ciphertext = sys.argv[1], int(sys.argv[2]), sys.argv[3]
with open(file_path, "r", encoding="utf-8") as f:
    payload = json.load(f)
for msg in payload:
    if msg.get("sender_id") == sender_id and msg.get("ciphertext_b64") == ciphertext:
        sys.exit(0)
sys.exit(1)
PY
}

cargo run -p server >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!

echo "Started server (pid=$SERVER_PID). Waiting for /healthz on $SERVER_PUBLIC_URL ..."
for _ in {1..180}; do
  if curl --silent --show-error --fail "$SERVER_PUBLIC_URL/healthz" >"$ARTIFACT_DIR/healthz.txt"; then
    echo "[OK] healthz" | tee -a "$CLIENT1_LOG" "$CLIENT2_LOG"
    mark_ok "server boot + /healthz"
    break
  fi
  sleep 1
done

curl --silent --show-error --fail "$SERVER_PUBLIC_URL/healthz" >"$ARTIFACT_DIR/healthz.txt"

CLIENT1_USERNAME="${CLIENT1_USERNAME:-local-user-1}"
CLIENT2_USERNAME="${CLIENT2_USERNAME:-local-user-2}"

request POST "$SERVER_PUBLIC_URL/login" 200 "$ARTIFACT_DIR/client1-login.json" \
  -H 'content-type: application/json' \
  --data "{\"username\":\"$CLIENT1_USERNAME\"}"
USER1_ID="$(extract_json "$ARTIFACT_DIR/client1-login.json" "user_id")"
echo "[OK] client1 login user_id=$USER1_ID" | tee -a "$CLIENT1_LOG"

request POST "$SERVER_PUBLIC_URL/login" 200 "$ARTIFACT_DIR/client2-login.json" \
  -H 'content-type: application/json' \
  --data "{\"username\":\"$CLIENT2_USERNAME\"}"
USER2_ID="$(extract_json "$ARTIFACT_DIR/client2-login.json" "user_id")"
echo "[OK] client2 login user_id=$USER2_ID" | tee -a "$CLIENT2_LOG"
mark_ok "two-user login"

request GET "$SERVER_PUBLIC_URL/guilds?user_id=$USER1_ID" 200 "$ARTIFACT_DIR/client1-guilds.json"
GUILD_ID="$(extract_json "$ARTIFACT_DIR/client1-guilds.json" "0.guild_id")"
echo "[OK] client1 guild selected guild_id=$GUILD_ID" | tee -a "$CLIENT1_LOG"

request POST "$SERVER_PUBLIC_URL/guilds/$GUILD_ID/invites?user_id=$USER1_ID" 200 "$ARTIFACT_DIR/client1-invite.json"
INVITE_CODE="$(extract_json "$ARTIFACT_DIR/client1-invite.json" "invite_code")"
echo "[OK] invite created" | tee -a "$CLIENT1_LOG"

request POST "$SERVER_PUBLIC_URL/guilds/join" 204 "$ARTIFACT_DIR/client2-join.txt" \
  -H 'content-type: application/json' \
  --data "{\"user_id\":$USER2_ID,\"invite_code\":\"$INVITE_CODE\"}"
echo "[OK] client2 joined guild_id=$GUILD_ID" | tee -a "$CLIENT2_LOG"

request GET "$SERVER_PUBLIC_URL/guilds/$GUILD_ID/channels?user_id=$USER1_ID" 200 "$ARTIFACT_DIR/client1-channels.json"
CHANNEL_ID="$(extract_json "$ARTIFACT_DIR/client1-channels.json" "0.channel_id")"
echo "[OK] client1 channel selected channel_id=$CHANNEL_ID" | tee -a "$CLIENT1_LOG"

request GET "$SERVER_PUBLIC_URL/guilds/$GUILD_ID/channels?user_id=$USER2_ID" 200 "$ARTIFACT_DIR/client2-channels.json"
echo "[OK] client2 channel selected channel_id=$CHANNEL_ID" | tee -a "$CLIENT2_LOG"
mark_ok "channel selection"

MESSAGE_B64="$(printf 'pre-e2ee-local-smoke-message' | base64 | tr -d '\n')"
request POST "$SERVER_PUBLIC_URL/messages" 200 "$ARTIFACT_DIR/client1-send-message.json" \
  -H 'content-type: application/json' \
  --data "{\"user_id\":$USER1_ID,\"guild_id\":$GUILD_ID,\"channel_id\":$CHANNEL_ID,\"ciphertext_b64\":\"$MESSAGE_B64\"}"
echo "[OK] client1 sent message" | tee -a "$CLIENT1_LOG"

request GET "$SERVER_PUBLIC_URL/channels/$CHANNEL_ID/messages?user_id=$USER2_ID&limit=20" 200 "$ARTIFACT_DIR/client2-messages.json"
contains_message "$ARTIFACT_DIR/client2-messages.json" "$USER1_ID" "$MESSAGE_B64"
echo "[OK] client2 received message" | tee -a "$CLIENT2_LOG"
mark_ok "message send/receive"

UPLOAD_FILE="$ARTIFACT_DIR/upload.bin"
printf 'pre-e2ee-upload-download-smoke' >"$UPLOAD_FILE"
request POST "$SERVER_PUBLIC_URL/files/upload?user_id=$USER1_ID&guild_id=$GUILD_ID&channel_id=$CHANNEL_ID&filename=smoke.bin&mime_type=application%2Foctet-stream" 200 "$ARTIFACT_DIR/client1-upload.json" \
  --data-binary "@$UPLOAD_FILE"
FILE_ID="$(extract_json "$ARTIFACT_DIR/client1-upload.json" "file_id")"
echo "[OK] upload file_id=$FILE_ID" | tee -a "$CLIENT1_LOG"

request GET "$SERVER_PUBLIC_URL/files/$FILE_ID?user_id=$USER2_ID" 200 "$ARTIFACT_DIR/client2-download.bin"
cmp -s "$UPLOAD_FILE" "$ARTIFACT_DIR/client2-download.bin"
echo "[OK] download content verified" | tee -a "$CLIENT2_LOG"
mark_ok "upload/download byte-equality"
