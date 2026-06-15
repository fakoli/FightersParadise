# Fighters Paradise — developer convenience Makefile.
#
# This is the canonical dev-workstation interface. Every target is a thin
# wrapper around a plain `cargo` command (there is no build magic hidden here),
# so anything you can do via `make` you can also do by hand. Run `make help`
# (the default target) for a self-documented list.
#
# SDL2 / wgpu are native dependencies. On macOS the linker needs Homebrew's
# libdir; the block below exports it via RUSTFLAGS *only when Homebrew exists*,
# so this Makefile is a no-op on Linux/CI. .cargo/config.toml already injects
# `-L /opt/homebrew/lib` for the aarch64-apple-darwin target; appending the
# brew prefix here is harmless (duplicate -L paths are fine) and also covers
# Intel macOS (/usr/local) where the config.toml target does not match.
#
# For the long-running windowed game (start/stop/restart/status), use
# scripts/fp.sh — a Makefile cannot cleanly supervise a detached GUI process.

# ---- macOS SDL2 linker path (portable, no-op on Linux) -----------------------
BREW_PREFIX := $(shell brew --prefix 2>/dev/null)
ifneq ($(BREW_PREFIX),)
export RUSTFLAGS := $(RUSTFLAGS) -L $(BREW_PREFIX)/lib
endif

# Run the default-launch character through the app. DEFAULT_DEF mirrors the
# hardcoded fallback in crates/fp-app/src/main.rs (relative to the repo root).
DEFAULT_DEF := test-assets/kfm/kfm.def

CARGO    ?= cargo
WORKSPACE := --workspace

.DEFAULT_GOAL := help

# ---- meta --------------------------------------------------------------------

.PHONY: help
help: ## Show this help (default target)
	@echo "Fighters Paradise — make targets:"
	@echo ""
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| sort \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "For the windowed game lifecycle (start/stop/restart/status) use: scripts/fp.sh"

# ---- build / run -------------------------------------------------------------

.PHONY: build
build: ## Build the whole workspace (debug)
	$(CARGO) build $(WORKSPACE)

.PHONY: run
run: ## Run fp-app: real KFM match if test-assets present, else the no-arg test pattern
	@if [ -e "$(DEFAULT_DEF)" ]; then \
		echo "==> test-assets found; launching two-KFM match"; \
		$(CARGO) run -p fp-app; \
	else \
		echo "==> $(DEFAULT_DEF) not found; launching no-arg test pattern (checkerboard)"; \
		$(CARGO) run -p fp-app; \
	fi

.PHONY: run-kfm
run-kfm: ## Run an explicit two-KFM match (requires test-assets/kfm/kfm.def)
	@if [ ! -e "$(DEFAULT_DEF)" ]; then \
		echo "error: $(DEFAULT_DEF) not found (test-assets is a local-only gitignored symlink)" >&2; \
		exit 1; \
	fi
	$(CARGO) run -p fp-app -- $(DEFAULT_DEF) $(DEFAULT_DEF)

.PHONY: run-sprite
run-sprite: ## Run the static SFF sprite viewer: make run-sprite SFF=path/to/file.sff [AIR=file.air]
	@if [ -z "$(SFF)" ]; then \
		echo "usage: make run-sprite SFF=path/to/file.sff [AIR=path/to/file.air]" >&2; \
		exit 2; \
	fi
	$(CARGO) run -p fp-app -- $(SFF) $(AIR)

# ---- test / quality ----------------------------------------------------------

.PHONY: test
test: ## Run the full workspace test suite
	$(CARGO) test $(WORKSPACE)

.PHONY: test-fast
test-fast: ## Run tests in the current crate only (CRATE=fp-vm), fast iteration
	@if [ -n "$(CRATE)" ]; then \
		$(CARGO) test -p $(CRATE); \
	else \
		echo "==> no CRATE set; running lib/bin unit tests only (skipping integration + doc tests)"; \
		$(CARGO) test $(WORKSPACE) --lib --bins; \
	fi

.PHONY: check
check: ## Type-check the workspace without producing binaries (fastest feedback)
	$(CARGO) check $(WORKSPACE) --all-targets

.PHONY: clippy
clippy: ## Lint with clippy, denying all warnings (matches CI)
	$(CARGO) clippy $(WORKSPACE) --all-targets -- -D warnings

.PHONY: fmt
fmt: ## Format all code in place (cargo fmt --all)
	$(CARGO) fmt --all

.PHONY: fmt-check
fmt-check: ## Check formatting without modifying files (cargo fmt --all --check)
	$(CARGO) fmt --all --check

.PHONY: doc
doc: ## Build and open API docs for the workspace
	$(CARGO) doc $(WORKSPACE) --no-deps --open

.PHONY: clean
clean: ## Remove the target/ build directory (cargo clean)
	$(CARGO) clean

# ---- aggregate ---------------------------------------------------------------

.PHONY: ci
ci: fmt-check clippy test ## Run the full local gate: fmt-check + clippy -D warnings + test
	@echo "==> local CI gate passed (fmt-check + clippy + test)"
	@echo "    NOTE: this mirrors GitHub CI, which now gates on cargo fmt --all --check"
	@echo "    (backlog CB3, done). Real-content KFM tests remain no-op on CI because"
	@echo "    test-assets is gitignored (known issue #36)."
