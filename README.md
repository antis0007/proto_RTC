# proto_RTC

A Rust monorepo skeleton for a self-hostable, barebones Discord replacement.

## License

This project is licensed under the MIT License.

## Architecture at a glance

- **Community Server (our backend)**: identity, guild/channel metadata, moderation, ciphertext relay/storage, file ciphertext storage, and LiveKit token minting.
- **LiveKit Server (self-hosted external backend)**: media plane for voice and screen sharing over WebRTC.
- **Client**: minimal desktop CLI/TUI plus shared client core.

The split keeps our server as the **control plane** and LiveKit as the **media plane**.

## Workspace layout

- `crates/shared`: shared domain types + protocol messages
- `crates/storage`: SQLite persistence (sqlx + migrations)
- `crates/livekit_integration`: LiveKit room naming + JWT token minting
- `crates/server_api`: transport-agnostic business logic
- `crates/server`: axum HTTP/WS server
- `crates/client_core`: protocol client + crypto boundary trait
- `apps/desktop`: minimal CLI/TUI entrypoint
- `apps/tools`: local admin/dev CLI

See docs in `docs/` for details.

## Quick start

```bash
cargo build
cargo test
cargo run -p server
```

Or use `make dev` / `just dev`.
