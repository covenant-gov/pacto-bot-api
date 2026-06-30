# pacto-bot-api development shortcuts

.PHONY: help fmt fmt-check clippy test build coverage validate deny clean run admin xtask-codegen install-hooks cross-setup cross-compile cross-compile-macos cross-compile-linux cross-compile-windows cross-compile-freebsd package

help: ## Show this help message
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

# Cross-compilation targets using cargo-zigbuild (zig handles C toolchains/linking).
# macOS host recommended. Install once: brew install zig cargo-zigbuild
# Linux host: install zig from https://ziglang.org/learn/getting-started/ then cargo install cargo-zigbuild
CROSS_MACOS_TARGETS := x86_64-apple-darwin aarch64-apple-darwin
CROSS_LINUX_TARGETS := x86_64-unknown-linux-musl aarch64-unknown-linux-musl
CROSS_WINDOWS_TARGETS := x86_64-pc-windows-gnu
CROSS_FREEBSD_TARGETS := x86_64-unknown-freebsd
CROSS_ALL_TARGETS := $(CROSS_MACOS_TARGETS) $(CROSS_LINUX_TARGETS) $(CROSS_WINDOWS_TARGETS) $(CROSS_FREEBSD_TARGETS)

cross-setup: ## Install rustup targets needed for cross-compilation
	rustup target add $(filter-out universal2-apple-darwin,$(CROSS_ALL_TARGETS))

cross-compile: cross-setup ## Build release binaries for all supported targets (requires zig + cargo-zigbuild)
	@for target in $(CROSS_ALL_TARGETS); do \
		echo "==> building $$target"; \
		cargo zigbuild --release --target $$target; \
	done
	@echo "Binaries written to target/<triple>/release/"

cross-compile-macos: cross-setup ## Build macOS x86_64 and arm64 binaries
	cargo zigbuild --release $(foreach t,$(CROSS_MACOS_TARGETS),--target $(t))

cross-compile-linux: cross-setup ## Build Linux x86_64 + arm64 static-musl binaries
	cargo zigbuild --release $(foreach t,$(CROSS_LINUX_TARGETS),--target $(t))

cross-compile-windows: cross-setup ## Build Windows x86_64 binary
	cargo zigbuild --release $(foreach t,$(CROSS_WINDOWS_TARGETS),--target $(t))

cross-compile-freebsd: cross-setup ## Build FreeBSD x86_64 binary
	cargo zigbuild --release $(foreach t,$(CROSS_FREEBSD_TARGETS),--target $(t))

package: cross-compile ## Build and package all release artifacts (tar/zip + sha256)
	@./scripts/package-release.sh

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

validate: fmt-check clippy ## Run fmt-check, clippy, and tests

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
