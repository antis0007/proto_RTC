# Initial Protocol

## Serialization

- WebSocket payloads: JSON
- HTTP payloads: JSON (except file upload/download bytes)

## Client -> Server requests

Defined in `shared::protocol::ClientRequest`:

- `Login { username }`
- `JoinGuild { guild_id }`
- `ListGuilds`
- `ListChannels { guild_id }`
- `SendMessage { channel_id, ciphertext_b64 }`
- `CreateInvite { guild_id }`
- `Kick { guild_id, target_user_id }`
- `Ban { guild_id, target_user_id }`
- `Mute { guild_id, target_user_id }`
- `RequestLiveKitToken { guild_id, channel_id, can_publish_mic, can_publish_screen }`

## Server -> Client events

Defined in `shared::protocol::ServerEvent`:

- `GuildUpdated`
- `ChannelUpdated`
- `MessageReceived`
- `UserKicked`
- `UserBanned`
- `UserMuted`
- `LiveKitTokenIssued`
- `Error(ApiError)`

## Event flow (text)

1. Client logs in over HTTP (`POST /login`)
2. Client opens WS (`GET /ws?user_id=...`)
3. Client sends `SendMessage` with ciphertext payload
4. Server checks mute/membership/ban, stores ciphertext, relays `MessageReceived`

## Event flow (voice/screen)

1. Client sends `RequestLiveKitToken`
2. Server validates membership/moderation and channel kind
3. Server mints JWT and returns `LiveKitTokenIssued`
4. Client joins LiveKit room using token

## HTTP route contract: MLS pending Welcome (MVP)

### Endpoint
- `GET /mls/welcome?user_id=<u64>&guild_id=<u64>&channel_id=<u64>`

### Access control
- Membership-gated endpoint.
- Caller must be authorized as `user_id` and be an active (non-banned) member for the target guild/channel.
- Unauthorized requests return `403 Forbidden`.

### Behavior
- Returns the latest unconsumed pending Welcome for `(guild_id, channel_id, user_id)`.
- Retrieval is one-time: load and mark consumed in the same DB transaction.
- If no pending unconsumed Welcome exists, return `404 Not Found`.

### `200 OK` response schema

```json
{
  "guild_id": 42,
  "channel_id": 9001,
  "user_id": 1007,
  "welcome_b64": "AAECAwQF...",
  "consumed_at": "2026-01-10T14:52:11Z"
}
```

Field types:
- `guild_id`: number (u64)
- `channel_id`: number (u64)
- `user_id`: number (u64)
- `welcome_b64`: string (base64 MLS Welcome bytes)
- `consumed_at`: string (RFC3339 UTC timestamp generated on consume)

### Failure payload examples

`404 Not Found` (no pending Welcome):

```json
{
  "error": {
    "code": "not_found",
    "message": "no pending welcome"
  }
}
```

`403 Forbidden` (membership/auth failure):

```json
{
  "error": {
    "code": "forbidden",
    "message": "user is not authorized for requested guild/channel"
  }
}
```

### Realtime hint event in MVP
- `ServerEvent::MlsWelcomeAvailable` is out of MVP scope (optional later optimization).
