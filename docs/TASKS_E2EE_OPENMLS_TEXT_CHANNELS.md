# Tasks: True E2EE for text channels with OpenMLS

## Goal
Implement end-to-end encryption for text messages per `(guild_id, channel_id)` using `crates/mls`, so the server stores only opaque ciphertext while clients transparently display plaintext.

## Scope and acceptance
- Channel-scoped OpenMLS groups are created/joined by clients.
- Message send path encrypts with MLS application messages.
- Message receive path decrypts MLS ciphertext before UI display.
- Server stores only encrypted message blobs for text content.
- Server provides pending Welcome retrieval per user/channel.
- MVP commit handling can be minimal (Welcome-first bootstrap), but interfaces should not block later commit fanout.

---

## Task 1 — Shared protocol additions for MLS bootstrap
**Files:** `crates/shared/src/protocol.rs` (and any HTTP DTOs in client/server crates)

1. Add payload structures for pending Welcome retrieval and transport:
   - `WelcomeResponse { guild_id, channel_id, user_id, welcome_b64, consumed }` (or equivalent)
2. Add optional client/server event(s) for MLS bootstrap if needed by realtime UX:
   - e.g. `ServerEvent::MlsWelcomeAvailable { guild_id, channel_id }` (optional for MVP)
3. Keep existing message schema unchanged for transport (`ciphertext_b64`) so encrypted MLS bytes flow through existing message APIs.

**Done when:** protocol compiles across workspace and types are consumable by client/server.

---

## Task 2 — Storage migrations for pending Welcome state
**Files:** `crates/storage/migrations/*`, `crates/storage/src/lib.rs`

1. Add migration table for pending welcomes, keyed to user/channel:
   - Columns: `id`, `guild_id`, `channel_id`, `user_id`, `welcome_bytes`, `created_at`, `consumed_at`
   - Index/constraint to efficiently load latest unconsumed welcome per `(guild_id, channel_id, user_id)`.
2. Add storage methods:
   - `insert_pending_welcome(guild_id, channel_id, user_id, welcome_bytes)`
   - `load_and_consume_pending_welcome(guild_id, channel_id, user_id)` (single transactional operation)
3. Add tests in storage crate validating:
   - latest welcome returned
   - consumed record is not returned again
   - isolation across guild/channel/user combinations

**Done when:** storage tests pass and migration works on clean DB.

---

## Task 3 — Server endpoint for pending Welcome retrieval
**Files:** `crates/server/src/main.rs`, `crates/server_api/src/lib.rs`

### MVP contract (normative)

#### Endpoint shape
- **Method/Path:** `GET /mls/welcome`
- **Required query params:**
  - `user_id` (u64)
  - `guild_id` (u64)
  - `channel_id` (u64)

#### Authorization and access checks
- Request is **membership-gated**:
  - requester must be the same principal as `user_id` (or equivalent authenticated identity binding).
  - requester must be an active (non-banned) member of `guild_id`.
  - requester must be authorized for `channel_id` within that guild.
- If any authorization check fails, return `403 Forbidden`.

#### Success response schema (`200 OK`)
- JSON object:
  - `guild_id: u64`
  - `channel_id: u64`
  - `user_id: u64`
  - `welcome_b64: string` (base64-encoded MLS Welcome bytes)
  - `consumed_at: string (RFC3339 UTC timestamp)`
- `consumed_at` represents the server-side timestamp from the same transactional read that consumed the pending record.
- For MVP, prefer `consumed_at` over a boolean `consumed`; it is unambiguous and auditable.

#### Not-found behavior (`404 Not Found`)
- Return `404` when no unconsumed pending Welcome exists for the exact tuple `(guild_id, channel_id, user_id)`.
- This includes first-time miss and already-consumed cases (idempotent from client perspective: “no pending welcome now”).

#### One-time consumption rule
- Retrieval must be **load + mark consumed in one transaction**.
- Exactly one successful `GET` may return a given pending Welcome.
- Concurrent reads for the same tuple must not produce duplicate successful consumptions.

#### Realtime hint event scope
- `ServerEvent::MlsWelcomeAvailable { guild_id, channel_id }` is **out of MVP required scope**.
- It may be added as an optimization later; polling `GET /mls/welcome` is the MVP bootstrap mechanism.

#### Response examples

Success (`200 OK`):

```json
{
  "guild_id": 42,
  "channel_id": 9001,
  "user_id": 1007,
  "welcome_b64": "AAECAwQF...",
  "consumed_at": "2026-01-10T14:52:11Z"
}
```

No pending welcome (`404 Not Found`):

```json
{
  "error": {
    "code": "not_found",
    "message": "no pending welcome"
  }
}
```

Authorization failure (`403 Forbidden`):

```json
{
  "error": {
    "code": "forbidden",
    "message": "user is not authorized for requested guild/channel"
  }
}
```

1. Add endpoint:
   - `GET /mls/welcome?user_id=...&guild_id=...&channel_id=...`
2. Authorization:
   - verify requesting user is active member of guild/channel before reading welcome.
3. Behavior:
   - return pending welcome for this user/channel if exists.
   - mark as consumed in same request/transaction.
   - return 404 when none pending.
4. Add endpoint tests covering:
   - success and consume semantics
   - unauthorized/non-member access rejection
   - no pending welcome case

**Done when:** endpoint is wired, tested, and documented in API routes.

---

## Task 4 — Client MLS session manager per channel
**Files:** `crates/client_core/src/lib.rs` (plus helper module if needed)

1. Introduce channel MLS state manager keyed by `(guild_id, channel_id)`:
   - holds/loads `MlsGroupHandle` (or wrapper) for active text channels.
2. On `select_channel(channel_id)`:
   - resolve `guild_id`.
   - if local group state exists: load/activate.
   - else bootstrap:
     - if current user is first guild/channel member: create group.
     - otherwise call `GET /mls/welcome` and join via `join_group_from_welcome`.
3. Persist/load MLS state using `MlsStore` implementation (`storage` crate).
4. Add clear client errors for uninitialized channels (no local group and no pending welcome).

**Done when:** selecting a channel initializes MLS group state deterministically for first and subsequent members.

---

## Task 5 — Encrypt send path with MLS
**Files:** `crates/client_core/src/lib.rs`

1. Replace plaintext passthrough on text send path:
   - in `send_message_with_attachment_impl`, encrypt text bytes via active channel MLS `encrypt_application`.
2. Base64-encode MLS ciphertext bytes into existing `ciphertext_b64` HTTP field.
3. Keep server message endpoint unchanged except that payload is now true MLS ciphertext.
4. Ensure no plaintext fallback is used in production path.

**Done when:** outbound text payloads are MLS ciphertext bytes only.

---

## Task 6 — Decrypt receive path before surfacing to UI
**Files:** `crates/client_core/src/lib.rs`, `apps/desktop_gui/src/main.rs`

1. In client_core receive paths (`ws` and `fetch_messages`):
   - decode base64 bytes
   - decrypt with active channel MLS `decrypt_application`
   - re-emit event payload that UI can render as plaintext safely.
2. Decide where plaintext lives:
   - preferred: add a client-local event containing decrypted text (avoid mutating wire payload schema).
3. Update desktop UI rendering to consume decrypted text field/event rather than direct base64 decode of `ciphertext_b64`.
4. Handle non-application MLS messages (e.g., commits) gracefully:
   - process state updates
   - avoid displaying empty/control messages as chat text.

**Done when:** UI displays plaintext from decrypted MLS payload while transport/storage remain ciphertext.

---

## Task 7 — Welcome generation on membership expansion (MVP)
**Files:** `crates/client_core/src/lib.rs`, optionally server API integration points

1. For MVP bootstrap, ensure an existing member can add a new member key package and generate Welcome:
   - fetch target user key package (`/mls/key_packages` existing API)
   - call `add_member(key_package_bytes)` on channel group
   - store generated welcome server-side as pending for target user/channel
2. Define trigger point for add-member flow:
   - minimal approach: on first channel interaction after guild join, an existing member performs add.
3. Persist any resulting local commit processing required for the adder.

**Done when:** newly joined user can retrieve a pending Welcome and join channel MLS group without manual steps.

---

## Task 8 — End-to-end integration tests (MVP acceptance)
**Files:** integration tests in `crates/client_core`, `crates/server`, or dedicated test harness

1. Two-user scenario in same guild/channel:
   - user A initializes/selects channel and sends text
   - user B joins via pending Welcome and decrypts messages
2. Assertions:
   - server DB `messages.ciphertext` does not equal plaintext bytes
   - clients render expected plaintext strings
3. Add regression test for Welcome one-time consumption.

**Done when:** acceptance criteria are automated and passing.

---

## Suggested execution order
1. Task 2 (storage)
2. Task 3 (server endpoint)
3. Task 1 (shared protocol, if needed by endpoint response shape)
4. Task 4 (client session manager/bootstrap)
5. Task 5 (send encryption)
6. Task 6 (receive decryption/UI)
7. Task 7 (welcome generation path)
8. Task 8 (integration acceptance)

## Notes / non-goals for this MVP
- Full commit fanout for every membership mutation can be deferred, but design interfaces so commit persistence/distribution can be added without protocol breakage.
- This plan targets **text channel E2EE** only; media E2EE (LiveKit) remains separate.
