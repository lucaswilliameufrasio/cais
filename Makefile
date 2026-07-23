CARGO := cargo
NEXTEST_THREADS := 4

.PHONY: all setup test test-ci test-docker format lint build dev upgrade clean services-up services-down help

all: format lint build test

setup:
	@echo "==> Verifying Rust toolchain..."
	@command -v rustup >/dev/null 2>&1 || { echo "error: rustup not found. Install from https://rustup.rs/"; exit 1; }
	@rustup toolchain list | grep -q stable || rustup toolchain install stable
	@echo "==> Installing rustfmt and clippy..."
	rustup component add rustfmt clippy 2>/dev/null; true
	@echo "==> Installing cargo-nextest..."
	@command -v cargo-nextest >/dev/null 2>&1 || cargo install cargo-nextest --locked
	@echo "==> Installing cargo-llvm-cov..."
	@command -v cargo-llvm-cov >/dev/null 2>&1 || cargo install cargo-llvm-cov --locked
	@echo "==> Installing cargo-watch..."
	@command -v cargo-watch >/dev/null 2>&1 || cargo install cargo-watch --locked
	@echo "==> Installing cargo-edit (cargo upgrade)..."
	@command -v cargo-upgrade >/dev/null 2>&1 || cargo install cargo-edit --locked
	@echo "==> Installing cargo-audit..."
	@command -v cargo-audit >/dev/null 2>&1 || cargo install cargo-audit --locked
	@echo "==> Installing cargo-deny..."
	@command -v cargo-deny >/dev/null 2>&1 || cargo install cargo-deny --locked
	@echo "==> Installing git-cliff..."
	@command -v git-cliff >/dev/null 2>&1 || cargo install git-cliff --locked
	@echo "==> Fetching dependencies..."
	$(CARGO) fetch
	@echo "==> Building to validate toolchain..."
	$(CARGO) build
	@echo ""
	@echo "Setup complete."

test:
	RUN_DOCKER_TESTS=1 $(CARGO) nextest run \
		--locked \
		--workspace \
		--all-targets \
		--all-features \
		--test-threads $(NEXTEST_THREADS)

test-ci:
	RUN_DOCKER_TESTS=1 $(CARGO) llvm-cov nextest \
		--locked \
		--workspace \
		--all-targets \
		--all-features \
		--test-threads $(NEXTEST_THREADS) \
		--lcov \
		--output-path target/llvm-cov/lcov.info \
		--html \
		--output-dir target/llvm-cov/html

test-docker:
	RUN_DOCKER_TESTS=1 $(CARGO) test --test postgres_integration -- --test-threads=1

format:
	$(CARGO) fmt --all --check

lint:
	$(CARGO) clippy --locked --all-targets --all-features -- -D warnings

build:
	$(CARGO) build --locked --all-targets

dev:
	$(CARGO) watch -x run

upgrade:
	$(CARGO) upgrade

clean:
	$(CARGO) clean

services-up:
	@if [ -f docker-compose.yml ] || [ -f compose.yml ] || [ -f compose.yaml ]; then \
		docker compose up -d; \
	else \
		echo "No docker-compose file found. Starting individual PostgreSQL containers:"; \
		echo "  docker run -d --rm -e POSTGRES_PASSWORD=postgres -p 55432:5432 postgres:17-alpine"; \
		echo "  docker run -d --rm -e POSTGRES_PASSWORD=postgres -p 55433:5432 postgres:18-alpine"; \
	fi

services-down:
	@if [ -f docker-compose.yml ] || [ -f compose.yml ] || [ -f compose.yaml ]; then \
		docker compose down --remove-orphans; \
	else \
		echo "Stopping all cais PostgreSQL containers..."; \
		docker ps --filter "name=db-provisioner-tui" --format '{{.ID}}' | xargs -r docker rm -f; \
	fi

help:
	@echo "Usage: make <target>"
	@echo ""
	@echo "Targets:"
	@echo "  all           format + lint + build + test (default)"
	@echo "  setup         Install all required tools"
	@echo "  test          Run ALL tests with nextest (unit + integration + Docker)"
	@echo "  test-ci       Run ALL tests with nextest and generate coverage (lcov + html)"
	@echo "  test-docker   Run only Docker integration tests"
	@echo "  format        cargo fmt --all --check"
	@echo "  lint          cargo clippy with -D warnings"
	@echo "  build         cargo build --locked --all-targets"
	@echo "  dev           cargo watch -x run"
	@echo "  upgrade        cargo upgrade (cargo-edit)"
	@echo "  clean         cargo clean"
	@echo "  services-up   Start Docker services (compose or ad-hoc)"
	@echo "  services-down Stop Docker services"
	@echo "  help          Show this help"
