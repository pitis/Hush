.DEFAULT_GOAL := help
SHELL := /bin/bash -o pipefail
XCB := xcodebuild -project app/Semaphore.xcodeproj -scheme Semaphore -destination 'platform=macOS,arch=arm64' -derivedDataPath build
# the test host app prints harmless linkd/XPC noise on stderr; keep only what matters
XCB_FILTER := { grep -vE 'com\.apple\.linkd|synchronousRemoteObjectProxy|Process Instance Registry|IDETestOperationsObserverDebug' || true; }

.PHONY: help setup core project app run release perf test check format lint worktree update card

help: ## List available commands
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}'

setup: ## One-time: tools, Rust target, git hooks
	brew install protobuf xcodegen
	rustup target add aarch64-apple-darwin
	git config core.hooksPath .githooks

## ── Build ────────────────────────────────────────────────────────────

core: ## Build the Rust core (debug) + Swift bindings + xcframework
	scripts/build-core.sh debug

project: ## Generate the Xcode project from app/project.yml
	cd app && xcodegen -q

app: core project ## Build the Debug app into build/
	$(XCB) -configuration Debug build -quiet 2>&1 | $(XCB_FILTER)

run: app ## Build and launch the Debug app
	open build/Build/Products/Debug/Semaphore.app

release: ## Build the Release app (release core: thin LTO, stripped) and install it in /Applications
	scripts/build-core.sh release
	cd app && xcodegen -q
	$(XCB) -configuration Release build -quiet 2>&1 | $(XCB_FILTER)
	@pkill -x Semaphore && sleep 1 || true
	ditto build/Build/Products/Release/Semaphore.app /Applications/Semaphore.app
	@echo "✓ /Applications/Semaphore.app"

perf: ## Measure the installed app against the M1 budget (size, cold launch, idle RAM and CPU)
	scripts/perf.sh

## ── Gate ─────────────────────────────────────────────────────────────

format: ## Format Rust and Swift sources
	cd core && cargo fmt
	xcrun swift-format format -i -r app/Semaphore app/SemaphoreTests

lint: ## rustfmt check, clippy -D warnings, swift-format lint --strict
	cd core && cargo fmt --check
	cd core && cargo clippy --all-targets -q -- -D warnings
	xcrun swift-format lint --strict -r app/Semaphore app/SemaphoreTests

test: core project ## Rust tests + XCTest
	cd core && cargo test -q
	$(XCB) test -quiet 2>&1 | $(XCB_FILTER)

check: lint test ## The full gate: run before every push

## ── Workflow ─────────────────────────────────────────────────────────

worktree: ## New worktree: make worktree name=<slug> branch=<type/slug> [base=origin/<stacked-on>]
	@test -n "$(name)" -a -n "$(branch)" || { echo "usage: make worktree name=<slug> branch=<type/slug> [base=origin/<branch>]"; exit 2; }
	scripts/worktree.sh "$(name)" "$(branch)" $(base)

update: ## In a worktree: merge origin/main (or the stacked base) into this branch
	scripts/update.sh

card: ## Move a board card: make card issue=<n> status="In progress"
	@test -n "$(issue)" -a -n "$(status)" || { echo 'usage: make card issue=<n> status="Todo|In progress|In review|Done|Needs decision"'; exit 2; }
	scripts/card.sh "$(issue)" "$(status)"
