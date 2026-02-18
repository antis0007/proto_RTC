# Tasks: Backend stability and functional validation (pre-E2EE hardening)

## Goal
Restore confidence that the backend is available and stable, and that end users can sign in, send/receive chat messages, and upload/download media without MLS/channel-state errors.

## Scope and acceptance
- GUI/CLI sign-in does not surface `Backend is unavailable` under normal startup flow.
- Server starts cleanly and required endpoints respond.
- Text chat works end-to-end after sign-in (no send/decrypt regressions).
- MLS channel state is initialized consistently (no missing-group runtime failures in normal usage).
- File/media upload/download works for authorized users.
- A repeatable smoke script/checklist exists and passes locally.

---

## Task 1 — Reproduce and instrument the "Backend is unavailable" sign-in failure
**Files:** `apps/desktop_gui/src/main.rs`, `scripts/test-local-stack.sh`, `scripts/test-local-stack.ps1`

1. Reproduce the sign-in failure using the current quick-start flow.
2. Add structured logs around backend worker startup, command queue creation, and queue disconnect paths.
3. Distinguish these failure classes in status text/logs:
   - backend worker crashed on startup
   - server URL unreachable
   - login HTTP/API error
4. Ensure the UI status message includes actionable hint text (e.g., "check server logs", "verify /healthz").

**Done when:** the failure can be reproduced deterministically and logs identify root cause category within one run.

---

## Task 2 — Validate server startup and baseline health checks
**Files:** `crates/server/src/main.rs`, `scripts/run-temp-server.sh`, `scripts/run-temp-server.ps1`

1. Confirm startup covers:
   - config/env parsing
   - SQLite initialization + migrations
   - route registration for login/guild/channel/messages/files/MLS
2. Add/verify startup logs for bind address and DB path.
3. Validate health probes:
   - `GET /healthz` returns 200
   - login route reachable from client host context
4. Add a startup regression check in local scripts so failures fail fast before GUI launch.

**Done when:** temp server script fails fast on startup regressions and exposes clear diagnostics.

---

## Task 3 — Sign-in and session flow hardening
**Files:** `crates/client_core/src/lib.rs`, `apps/desktop_gui/src/main.rs`, `crates/server/src/main.rs`

1. Validate sign-in round trip from GUI and CLI clients.
2. Ensure post-login state is initialized in order:
   - user/session set
   - guild list loaded
   - channel map populated
3. Add regression checks for sign-in edge cases:
   - empty username
   - unreachable server URL
   - valid sign-in after transient server restart
4. Prevent stale/disconnected command queue from being reused after backend task failure; restart/recreate worker if needed.

**Done when:** sign-in works repeatedly without requiring GUI restart, and failures recover cleanly.

---

## Task 4 — Text messaging functional validation
**Files:** `crates/client_core/src/lib.rs`, `crates/server/src/api/mod.rs`, `crates/server/src/main.rs`

1. Validate send path after sign-in + channel selection:
   - no runtime error from active context resolution
   - no HTTP 4xx/5xx in normal member flow
2. Validate receive path:
   - latest messages load via HTTP
   - realtime WS events are rendered
3. Add/extend tests for:
   - message send success (two users in same channel)
   - fetch history after send
   - unauthorized send rejection for non-members

**Done when:** users can chat bidirectionally after sign-in with passing automated checks.

---

## Task 5 — MLS channel-state sanity validation (pre-hardening)
**Files:** `crates/client_core/src/lib.rs`, `crates/client_core/src/mls_session_manager.rs`, `crates/mls/src/lib.rs`

1. Validate channel selection initializes/loads MLS state for normal flows.
2. Add regression tests ensuring no "missing MLS group" error during:
   - select channel
   - send message after select
   - fetch/decrypt messages in selected channel
3. Verify onboarding path:
   - member add attempts do not crash client if key package/welcome unavailable
   - pending welcome fetch + join path remains non-fatal when none exists
4. Ensure errors that do occur are surfaced as actionable client events, not silent failures.

**Done when:** standard chat flow in joined channels does not produce missing-group MLS errors.

---

## Task 6 — Media/file upload and download validation
**Files:** `crates/server/src/main.rs`, `crates/server/src/api/mod.rs`, `crates/storage/src/lib.rs`, `apps/desktop_gui/src/main.rs`

1. Validate upload request path end-to-end (GUI and/or client_core):
   - accepted for authorized member
   - rejected for non-member
2. Validate download path and attachment metadata rendering.
3. Add automated checks for:
   - successful upload + subsequent download byte equality
   - permission-gated rejection
4. Verify UI handles upload errors without breaking chat session state.

**Done when:** media/file sharing is functional for members and correctly denied otherwise.

---

## Task 7 — End-to-end local smoke suite and release gate
**Files:** `scripts/test-local-stack.sh`, `scripts/test-local-stack.ps1`, optionally `justfile`/`Makefile`

1. Extend smoke test to include:
   - server boot + `/healthz`
   - two-user login
   - channel select
   - message send/receive
   - one upload/download check
2. Save logs/artifacts per run for server and both clients.
3. Define a single "pre-E2EE-hardening" command that must pass before resuming deeper MLS/E2EE tasks.

**Done when:** a single reproducible command validates backend availability + chat/media baseline.

---

## Suggested execution order
1. Task 1 (reproduce and classify backend unavailable failure)
2. Task 2 (server startup/health)
3. Task 3 (sign-in/session resilience)
4. Task 4 (chat validation)
5. Task 6 (media validation)
6. Task 5 (MLS sanity validation)
7. Task 7 (smoke gate)

## Exit criteria before returning to E2EE hardening tasks
- No `Backend is unavailable` during normal sign-in flow.
- Two local users can sign in, chat, and exchange at least one attachment.
- No missing MLS group errors in standard channel usage.
- Smoke suite passes on both Linux/macOS shell script and PowerShell script variants.
