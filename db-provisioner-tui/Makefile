.PHONY: all test lint format

all: lint format test

test:
	cargo test --lib
	RUN_DOCKER_TESTS=1 cargo test --test postgres_integration

lint:
	cargo clippy --all-targets -- -D warnings

format:
	cargo fmt --check
