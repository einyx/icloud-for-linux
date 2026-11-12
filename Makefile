# icloud-for-linux — development harness
#
# Quick start:
#   make check      # fmt + clippy + tests
#   make run        # run the CLI locally
#   make docker-dev # shell inside the dev container
#
# Everything is also runnable inside the dev container:
#   docker compose run --rm dev make check

SHELL := /bin/bash
.DEFAULT_GOAL := help

BINARY      := icloudctl
PKG         := icloud-for-linux
VERSION     := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
TARGET_DIR  := target
DIST_DIR    := dist
REPO        := einyx/icloud-for-linux
CARGO       ?= cargo

# Keep release artifacts reproducible.
export SOURCE_DATE_EPOCH ?= $(shell git log -1 --pretty=%ct 2>/dev/null || date +%s)

.PHONY: help
help: ## Show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

# ─────────────────────────────── build ───────────────────────────────
.PHONY: deps
deps: ## Fetch crate dependencies
	$(CARGO) fetch

.PHONY: build
build: ## Debug build
	$(CARGO) build

.PHONY: release
release: ## Release build
	$(CARGO) build --release

.PHONY: run
run: ## Run the CLI (ARGS="--help")
	$(CARGO) run -- $(ARGS)

.PHONY: install
install: release ## Install the binary to ~/.local/bin
	install -Dm755 $(TARGET_DIR)/release/$(BINARY) $(HOME)/.local/bin/$(BINARY)
	@echo "installed ~/.local/bin/$(BINARY)"

# ───────────────────────────── quality ───────────────────────────────
.PHONY: fmt
fmt: ## Format the code
	$(CARGO) fmt --all

.PHONY: fmt-check
fmt-check: ## Verify formatting (CI)
	$(CARGO) fmt --all -- --check

.PHONY: lint
lint: ## Run clippy
	$(CARGO) clippy --all-targets --all-features -- -D warnings

.PHONY: test
test: ## Run the test suite
	$(CARGO) test --all-targets

.PHONY: audit
audit: ## Dependency vulnerability audit
	$(CARGO) audit || true

.PHONY: check
check: fmt-check lint test ## fmt + clippy + tests

# ─────────────────────────── docker harness ───────────────────────────
.PHONY: docker-build
docker-build: ## Build the production container image
	docker build -t $(PKG):$(VERSION) -t $(PKG):latest .

.PHONY: docker-dev
docker-dev: ## Shell inside the dev container
	docker compose run --rm dev bash

.PHONY: docker-check
docker-check: ## Run fmt + clippy + tests inside the dev container
	docker compose run --rm dev make check

.PHONY: docker-fuse
docker-fuse: ## Run the FUSE mount in a container (requires /dev/fuse)
	docker compose --profile fuse up --build mount

.PHONY: docker-shell
docker-shell: ## Open a shell in the built production image
	docker compose run --rm --entrypoint bash build

.PHONY: docker-clean
docker-clean: ## Remove container images/volumes
	docker compose down -v --remove-orphans

# ─────────────────────────── distribution ───────────────────────────
.PHONY: dist
dist: release ## Produce a release tarball in dist/
	@mkdir -p $(DIST_DIR)
	@tar -czf $(DIST_DIR)/$(PKG)-$(VERSION)-linux-x86_64.tar.gz \
		-C $(TARGET_DIR)/release $(BINARY) \
		-C . LICENSE README.md config.example.yaml
	@echo "→ $(DIST_DIR)/$(PKG)-$(VERSION)-linux-x86_64.tar.gz"

.PHONY: tag
tag: ## Create an annotated release tag
	git tag -a v$(VERSION) -m "Release v$(VERSION)"

.PHONY: publish
publish: dist tag ## Push the tag and publish the GitHub release
	git push origin v$(VERSION)
	gh release create v$(VERSION) $(DIST_DIR)/*.tar.gz \
		--repo $(REPO) \
		--title "v$(VERSION)" \
		--generate-notes

.PHONY: clean
clean: ## Remove build artifacts
	$(CARGO) clean
	rm -rf $(DIST_DIR)
