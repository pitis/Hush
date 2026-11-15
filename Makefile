.DEFAULT_GOAL := help
SHELL := /bin/bash -o pipefail
XCB := xcodebuild -project app/Hush.xcodeproj -scheme Hush -destination 'platform=macOS,arch=arm64' -derivedDataPath build
# the test host app prints harmless linkd/XPC noise on stderr; keep only what matters
XCB_FILTER := { grep -vE 'com\.apple\.linkd|synchronousRemoteObjectProxy|Process Instance Registry|IDETestOperationsObserverDebug' || true; }

.PHONY: help setup core project app run bundle release publish perf desktop desktop-run test check format lint worktree update card

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
	open build/Build/Products/Debug/Hush.app

bundle: ## Build the Release app (release core: thin LTO, stripped) into build/
	scripts/build-core.sh release
	cd app && xcodegen -q
	$(XCB) -configuration Release build -quiet 2>&1 | $(XCB_FILTER)

release: bundle ## Build the Release app, quit the running one and install it in /Applications
	@osascript -e 'quit app id "dev.pitis.hush"' 2>/dev/null; sleep 1
	@# the app's old name: Hush moves its data over on first launch
	@osascript -e 'quit app id "dev.pitis.semaphore"' 2>/dev/null; rm -rf /Applications/Semaphore.app
	rm -rf /Applications/Hush.app
	ditto build/Build/Products/Release/Hush.app /Applications/Hush.app
	@# macOS refreshes a cached app icon only when the bundle's own date changes; ditto keeps the old one
	touch /Applications/Hush.app
	@echo "✓ /Applications/Hush.app"

publish: ## Publish origin/main's Release app as a GitHub release: make publish [version=0.2]
	scripts/publish.sh $(version)

perf: ## Measure the installed app against the M1 budget (size, cold launch, idle RAM and CPU)
	scripts/perf.sh

desktop: ## Build the desktop app (GPUI: Windows, Linux, macOS) into core/target/debug/hush
	cd core && cargo build -p hush-desktop

desktop-run: ## Build and launch the desktop app
	cd core && cargo run -p hush-desktop

## ── Gate ─────────────────────────────────────────────────────────────

format: ## Format Rust and Swift sources
	cd core && cargo fmt
	xcrun swift-format format -i -r app/Hush app/HushTests

lint: ## rustfmt check, clippy -D warnings, swift-format lint --strict
	cd core && cargo fmt --check
	cd core && cargo clippy --all-targets -q -- -D warnings
	@# the desktop's window tests build only with this feature
	cd core && cargo clippy --all-targets --features hush-desktop/snapshot -q -- -D warnings
	xcrun swift-format lint --strict -r app/Hush app/HushTests

test: core project ## Rust tests + XCTest
	@# with the desktop's window tests, which build only with this feature (workspace-wide, so the
	@# core's dependencies keep the features of the plain build and aren't compiled twice)
	cd core && cargo test --features hush-desktop/snapshot -q
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
