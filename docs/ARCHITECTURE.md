# Architecture

## Two-backend model

### 1) Community Server (control plane)

Responsibilities:

- User login and identity bootstrapping
- Guild/channel/membership/moderation state
- Real-time text message relay over WebSocket
- Ciphertext-only message/file persistence
- LiveKit access token minting with scoped grants

### 2) LiveKit Server (media plane)

Responsibilities:

- Voice rooms
- Screen sharing rooms
- WebRTC signaling/media handling

The client requests a LiveKit token from the Community Server, then connects directly to LiveKit.

## Why split control plane and media plane?

- Keeps our backend small and explainable
- Delegates complex realtime media concerns to a purpose-built SFU
- Preserves a clear trust boundary: our DB stores only opaque ciphertext blobs for text/files
- Makes self-hosting practical with `docker compose`

## Layering

- **Transport** (`crates/server`): HTTP + WebSocket only
- **Logic** (`crates/server_api`): permission checks, moderation, orchestration
- **Storage** (`crates/storage`): SQL access + migrations
- **Protocol/Domain** (`crates/shared`): request/event/error contracts
