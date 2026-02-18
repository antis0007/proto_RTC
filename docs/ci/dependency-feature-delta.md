# Dependency Feature Delta (Tokio/Axum/GUI)

This document captures the feature-scope audit and resulting dependency delta.

## Commands used

```bash
cargo tree -e features > /tmp/cargo-tree-before.txt
cargo tree -e features > /tmp/cargo-tree-after.txt
rg -n 'tokio feature "full"' /tmp/cargo-tree-before.txt
rg -n 'tokio feature "full"' /tmp/cargo-tree-after.txt
rg -n 'axum feature "(ws|multipart|macros)"' /tmp/cargo-tree-before.txt
rg -n 'axum feature "(ws|multipart|macros)"' /tmp/cargo-tree-after.txt
rg -n 'image feature "(gif|webp|bmp|png|jpeg)"' /tmp/cargo-tree-before.txt
rg -n 'image feature "(gif|webp|bmp|png|jpeg)"' /tmp/cargo-tree-after.txt
```

## High-level delta

- `tokio/full` occurrences in the workspace feature graph:
  - before: **7**
  - after: **0**
- `axum` extra features:
  - before: `macros`, `multipart`, `ws`
  - after: `ws` only (server crate)
- GUI image formats:
  - before: `png`, `jpeg`, `gif`, `webp`, `bmp`
  - after (default): `png`, `jpeg`
  - optional: `gif`, `webp`, `bmp` via `image-format-extended`

## Tokio feature scope by crate

- `crates/client_core`: `rt`, `sync`, `time` (+ test-only `macros`, `rt-multi-thread`, `net`)
- `crates/livekit_integration`: `sync`
- `crates/server`: `macros`, `rt-multi-thread`, `net`, `sync`
- `crates/mls` (dev only): `macros`, `rt-multi-thread`, `sync`
- `crates/storage`: no runtime tokio dependency; test-only `macros`, `rt-multi-thread`
- `apps/desktop`: `macros`, `rt-multi-thread`
- `apps/desktop_gui`: `rt-multi-thread`, `fs`
- `apps/tools`: `macros`, `rt-multi-thread`

## CI enforcement

- Added: `scripts/ci/check_tokio_full.py`
- Wired into `.github/workflows/ci.yml` as `Guard tokio/full in non-runtime crates`.
