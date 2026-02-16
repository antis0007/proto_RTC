.PHONY: server gui cli tools dev test-local-stack test fmt lint

server:
	bash scripts/run-server.sh

gui:
	bash scripts/run-gui.sh

cli:
	bash scripts/run-cli.sh

tools:
	bash scripts/run-tools.sh

dev: server

test-local-stack:
	bash scripts/test-local-stack.sh

test:
	cargo test --workspace --all-targets

fmt:
	cargo fmt --all

lint:
	cargo clippy --workspace --all-targets -- -D warnings
