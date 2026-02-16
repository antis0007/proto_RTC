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

## Quick Start (Local)

1. Copy the sample environment file and adjust values if needed:

```bash
cp .env.example .env
```

2. Start the server (creates `./data` and `./logs` automatically):

```bash
./scripts/run-server.sh
# Windows PowerShell: ./scripts/run-server.ps1
```

3. Start clients in separate terminals:

```bash
./scripts/run-cli.sh
./scripts/run-gui.sh
```

### One-PC workflow (server + two clients)

- Terminal 1: `./scripts/run-server.sh`
- Terminal 2: `./scripts/run-cli.sh`
- Terminal 3: `./scripts/run-cli.sh -- --username second-user`

Defaults connect clients to `http://127.0.0.1:8443`.

### Two-PC LAN workflow

On the server machine:

```bash
SERVER_BIND=0.0.0.0:8443 SERVER_PUBLIC_URL=http://<LAN_IP>:8443 ./scripts/run-server.sh
```

On each client machine:

```bash
SERVER_PUBLIC_URL=http://<LAN_IP>:8443 ./scripts/run-cli.sh
```

Allow inbound TCP port `8443` in your firewall.

### Developer helpers

- `just server` / `make server`
- `just gui` / `make gui`
- `just cli` / `make cli`
- `just dev` / `make dev` (starts server)
