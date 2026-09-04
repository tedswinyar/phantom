# Phantom — root Makefile. Layer-aware: targets for pruned layers
# (no swift/, no website/) degrade to a clear message, not an error spew.

.PHONY: build test verify release-build app dmg website serve-website clean help

help: ## Show this help
	@grep -E '^[a-z-]+:.*##' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-16s %s\n", $$1, $$2}'

build: ## Debug-build the Rust workspace (and Swift app if present)
	cd rust && cargo build --workspace
	@if [ -d swift ]; then cd swift && swift build; fi

release-build: ## Release-build the Rust workspace
	cd rust && cargo build --workspace --release

test: verify ## Alias for verify

verify: ## Run all repo health checks (auto-detects layers)
	./scripts/verify.sh

app: ## Assemble the .app bundle (requires swift/)
	@if [ ! -d swift ]; then echo "no swift/ layer in this project"; exit 1; fi
	./scripts/build-app.sh

dmg: ## Build the distributable DMG (requires swift/ + release config)
	@if [ ! -d swift ]; then echo "no swift/ layer in this project"; exit 1; fi
	./scripts/build-dmg.sh

website: ## Build the website (requires website/)
	@if [ ! -d website ]; then echo "no website/ layer in this project"; exit 1; fi
	cd website && hugo build

serve-website: ## Serve the website locally with live reload
	@if [ ! -d website ]; then echo "no website/ layer in this project"; exit 1; fi
	cd website && hugo server

start: ## Start the API server in the dev profile
	./scripts/start.sh

clean: ## Remove build products
	cd rust && cargo clean
	@if [ -d swift ]; then cd swift && swift package clean; fi
	rm -rf .runtime build dist
