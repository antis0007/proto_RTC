# Threat Model (Initial)

## Ciphertext-only goal

For text and file content, server-side storage should hold only opaque ciphertext bytes + metadata.

## What Community Server can see

- Account identifiers and guild membership
- Channel structure and moderation actions
- Message/file metadata (sender, channel, timestamps, sizes, mime hints)
- Ciphertext blobs (not plaintext, once real E2EE lands)

## What Community Server should not see (target state)

- Message plaintext
- File plaintext
- Room media contents (handled by LiveKit)

## Current prototype limitation

The initial client wraps plaintext into `ciphertext` fields. This is a temporary bootstrapping mode.

## LiveKit trust boundary

- Community Server issues short-lived access tokens
- LiveKit handles media transport and SFU behavior
- Future hardening: rotate API secrets, short TTL tokens, per-room grants

## Out-of-scope for now

- Key transparency / device verification
- Deniability properties
- Perfect forward secrecy for all client sessions
