# Hyperion developer workflow. Everything runs inside Docker because Glommio
# (io_uring) is Linux-only and the host is macOS.

DC     := docker compose
RUN    := $(DC) run --rm app
PACKAGE := hyperion-cli

.PHONY: help build run test shell smoke fmt clippy logs down

help:
	@echo "Targets:"
	@echo "  build   cargo build --workspace"
	@echo "  run     cargo run -p $(PACKAGE) -- smoke"
	@echo "  smoke   same as run"
	@echo "  test    cargo test --workspace -j 1 (serial link avoids V8 OOM)"
	@echo "  shell   interactive bash in the dev container"
	@echo "  fmt     cargo fmt --check"
	@echo "  clippy  cargo clippy --workspace -- -D warnings"
	@echo "  logs    docker compose logs -f"
	@echo "  down    docker compose down"

build:
	$(RUN) cargo build --workspace

run:
	$(RUN) cargo run -p $(PACKAGE) -- smoke

smoke:
	$(RUN) cargo run -p $(PACKAGE) -- smoke

test:
	$(RUN) cargo test --workspace -j 1 -- --test-threads=1

shell:
	$(RUN) bash

fmt:
	$(RUN) cargo fmt --all -- --check

clippy:
	$(RUN) cargo clippy --workspace --all-targets -- -D warnings

logs:
	$(DC) logs -f

down:
	$(DC) down
