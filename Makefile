.PHONY: dev test fmt lint

dev:
	cargo run -p server

test:
	cargo test --workspace --all-targets

fmt:
	cargo fmt --all

lint:
	cargo clippy --workspace --all-targets -- -D warnings
