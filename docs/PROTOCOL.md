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
