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

## Prerequisites (all platforms)

1. Install Rust (stable) and Cargo: <https://rustup.rs>
2. Verify toolchain:

   ```bash
   cargo --version
   rustc --version
   ```

3. From the repository root, create a local env file:

   ```bash
   cp .env.example .env
   ```

   On Windows PowerShell:

   ```powershell
   Copy-Item .env.example .env
   ```

## Script map (no ambiguity)

Use **bash scripts (`.sh`) on Linux/macOS**, and **PowerShell scripts (`.ps1`) on Windows**.

| Purpose | Linux/macOS | Windows PowerShell |
|---|---|---|
| Start server (foreground) | `./scripts/run-server.sh` | `./scripts/run-server.ps1` |
| Start client (foreground) | `./scripts/run-cli.sh` | `./scripts/run-cli.ps1` |
| Start GUI (if available) | `./scripts/run-gui.sh` | `./scripts/run-gui.ps1` |
| Start tools app | `./scripts/run-tools.sh` | `./scripts/run-tools.ps1` |
| LAN host server helper | `./scripts/launch-lan-server.sh <LAN_IP> [PORT]` | `./scripts/launch-lan-server.ps1 <LAN_IP> [PORT]` |
| Remote client helper | `./scripts/run-remote-client.sh <SERVER_URL> [USERNAME]` | `./scripts/run-remote-client.ps1 <SERVER_URL> [USERNAME]` |
| One-machine smoke test (server + 2 clients) | `./scripts/test-local-stack.sh` | `./scripts/test-local-stack.ps1` |

## Launch workflows

### A) One computer: server + 2 clients (automated smoke test)

This is the fastest reproducible test path.

- Linux/macOS:

  ```bash
  ./scripts/test-local-stack.sh
  ```

- Windows PowerShell:

  ```powershell
  ./scripts/test-local-stack.ps1
  ```

What it does:
- starts the server,
- waits for `GET /healthz`,
- runs client #1 and client #2 with unique usernames,
- writes logs to `logs/test-local-*.log`,
- stops the server process.

### B) One computer: manual server + clients (separate terminals)

Use this when you want interactive control.

1. Terminal A (server):

   - Linux/macOS: `./scripts/run-server.sh`
   - Windows: `./scripts/run-server.ps1`

2. Terminal B (client 1):

   - Linux/macOS: `./scripts/run-cli.sh`
   - Windows: `./scripts/run-cli.ps1`

3. Terminal C (client 2, unique username):

   - Linux/macOS: `CLI_USERNAME=second-user ./scripts/run-cli.sh`
   - Windows: `$env:CLI_USERNAME='second-user'; ./scripts/run-cli.ps1`

Default URL is `http://127.0.0.1:8443`.

### C) Two computers: server on PC-1, client on PC-2

#### PC-1 (server host)

1. Find PC-1 LAN IP (example: `192.168.1.42`).
2. Start server bound to all interfaces:

   - Linux/macOS:

     ```bash
     ./scripts/launch-lan-server.sh 192.168.1.42 8443
     ```

   - Windows:

     ```powershell
     ./scripts/launch-lan-server.ps1 192.168.1.42 8443
     ```

3. Open inbound TCP port `8443` in firewall.

#### PC-2 (remote client)

- Linux/macOS:

  ```bash
  ./scripts/run-remote-client.sh http://192.168.1.42:8443 remote-user
  ```

- Windows:

  ```powershell
  ./scripts/run-remote-client.ps1 http://192.168.1.42:8443 remote-user
  ```

## Environment variables used by scripts

- `SERVER_BIND` (default `127.0.0.1:8443`): server bind address.
- `SERVER_PUBLIC_URL` (default `http://127.0.0.1:8443`): URL clients use.
- `DATABASE_URL` (default `sqlite://./data/server.db`).
- `CLI_USERNAME` (default `local-user`).
- `CLIENT1_USERNAME` / `CLIENT2_USERNAME`: used by `test-local-stack.*`.

Variables can be set in `.env` (loaded by scripts) or inline in terminal.

## Developer helpers

- `just server` / `make server`
- `just gui` / `make gui`
- `just cli` / `make cli`
- `just tools` / `make tools`
- `just test-local-stack` / `make test-local-stack`
- `just dev` / `make dev` (starts server)
