# Build and check recipes. `make test` runs exactly what CI runs.
.PHONY: all frontend build release test ci dev-backend dev-frontend clean

all: build

# Build the dashboard into frontend/dist (embedded into the binary by crates/local-ai-gateway/build.rs).
frontend:
	cd frontend && npm ci && npm run build

build:
	cargo build --workspace

release: frontend
	cargo build --release --locked

# The CI gates: formatting, zero clippy warnings, every test, frontend types and lint.
test:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets --locked -- -D warnings
	cargo test --workspace --locked
	cd frontend && npm run typecheck && npm run lint

ci: test

# A scratch gateway for development: dashboard on 127.0.0.1:8788 (TLS off), proxy listeners on
# 41337/41434 pointing at the real backends, data under ./scratch (gitignored).
dev-backend:
	mkdir -p scratch/data
	test -f scratch/config.toml || cp scripts/dev-config.toml scratch/config.toml
	cargo run -p local-ai-gateway -- --config scratch/config.toml --data-dir scratch/data --headless

# Vite with hot reload on 5188, proxying /api and /ws to the scratch gateway.
dev-frontend:
	cd frontend && npm run dev

clean:
	cargo clean
	rm -rf frontend/dist
