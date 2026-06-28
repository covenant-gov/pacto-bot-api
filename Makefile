# pacto-bot-api development shortcuts

.PHONY: help fmt fmt-check clippy test build coverage validate deny clean run admin xtask-codegen install-hooks

help: ## Show this help message
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

fmt: ## Format all Rust code
	cargo fmt --all

fmt-check: ## Check formatting without writing
	cargo fmt --all -- --check

clippy: ## Run clippy lints across the workspace
	cargo clippy --all-targets --all-features --workspace -- -D warnings

test: ## Run the full test suite
	cargo test --all-targets --all-features

build: ## Build all targets
	cargo build --all-targets

coverage: ## Generate test coverage report (requires cargo-llvm-cov)
	@if command -v cargo-llvm-cov >/dev/null 2>&1; then \
		cargo llvm-cov --all-features --workspace --lcov --output-path lcov.info; \
	else \
		echo "cargo-llvm-cov not installed. Install with: cargo install cargo-llvm-cov"; \
		exit 1; \
	fi

validate: fmt-check clippy test ## Run fmt-check, clippy, and tests

deny: ## Run cargo-deny audit gates
	cargo deny check

clean: ## Remove build artifacts
	cargo clean

run: ## Run the daemon binary
	cargo run --bin pacto-bot-api

admin: ## Run the admin CLI binary
	cargo run --bin pacto-bot-admin

xtask-codegen: ## Regenerate Rust types from schemas/
	cargo xtask codegen

install-hooks: ## Install the pre-commit hook
	cp scripts/pre-commit.sh .git/hooks/pre-commit
	chmod +x .git/hooks/pre-commit
	@echo "Pre-commit hook installed."
