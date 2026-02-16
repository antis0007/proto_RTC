# Deployment

## Local development

### Requirements

- Rust stable toolchain
- Docker + docker compose

### Run app locally

```bash
cargo run -p server
cargo run -p desktop -- --server-url http://127.0.0.1:8080 --username alice
```

### Run compose stack

```bash
docker compose -f deploy/docker/docker-compose.yml up --build
```

This starts:

- `community-server` (our Rust backend)
- `livekit` (self-hosted external media backend)

> LiveKit production setup requires additional config and hardening. Refer to official LiveKit self-hosting docs when moving beyond local development.

## Storage choice

This skeleton uses **sqlx + SQLite**:

- compile-time checked query macros can be introduced later
- async-friendly in Tokio runtime
- straightforward migration story via `sqlx::migrate!`
