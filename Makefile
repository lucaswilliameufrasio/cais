.PHONY: all setup test fmt lint check clean run help

CARGO := cargo

all: check test

setup:
	@echo "==> Verifying Rust toolchain..."
	@command -v rustup >/dev/null 2>&1 || { echo "error: rustup not found. Install from https://rustup.rs/"; exit 1; }
	@rustup toolchain list | grep -q stable || rustup toolchain install stable
	@echo "==> Installing rustfmt and clippy..."
	rustup component add rustfmt clippy 2>/dev/null; true
	@echo "==> Fetching dependencies..."
	$(CARGO) fetch
	@echo "==> Building to validate toolchain..."
	$(CARGO) build
	@echo ""
	@echo "Setup complete."

test:
	$(CARGO) test --lib
	$(CARGO) test --test postgres_integration --no-run

test-docker:
	RUN_DOCKER_TESTS=1 $(CARGO) test --test postgres_integration -- --test-threads=1

fmt:
	$(CARGO) fmt --all --check

lint:
	$(CARGO) clippy --all-targets -- -D warnings

check: fmt lint
	$(CARGO) check --all-targets

run:
	$(CARGO) run $(ARGS)

release:
	$(CARGO) build --release $(ARGS)

clean:
	$(CARGO) clean

help:
	@echo "Usage: make <target>"
	@echo ""
	@echo "Targets:"
	@echo "  setup        Install required tools and prepare environment"
	@echo "  test         Run library tests (no Docker)"
	@echo "  test-docker  Run integration tests (requires Docker)"
	@echo "  fmt          Check code formatting"
	@echo "  lint         Run clippy lints"
	@echo "  check        Check compilation, fmt, and lint"
	@echo "  run          Run the application (ARGS=... for extra flags)"
	@echo "  release      Build release binary"
	@echo "  clean        Remove build artifacts"
	@echo "  help         Show this help"
