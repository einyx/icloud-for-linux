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
# Ask cargo where it puts the build output: it may be redirected by a
# configuration file, so the path cannot be guessed.
TARGET_DIR  ?= $(shell cargo metadata --format-version 1 --no-deps 2>/dev/null \
	| sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
DIST_DIR    := dist
REPO        := einyx/icloud-for-linux
CARGO       ?= cargo
PREFIX      ?= /usr/local
DESTDIR     ?=
MANDIR      ?= $(PREFIX)/share/man
DATADIR     ?= $(PREFIX)/share
SYSCONFDIR  ?= /etc
RPM_TOPDIR  ?= $(CURDIR)/dist/rpmbuild

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
install: release completions ## Install the binary, the man page and the completions
	install -Dm755 $(TARGET_DIR)/release/$(BINARY) $(DESTDIR)$(PREFIX)/bin/$(BINARY)
	install -Dm644 docs/icloudctl.1 $(DESTDIR)$(MANDIR)/man1/icloudctl.1
	install -Dm644 config.example.yaml \
		$(DESTDIR)$(SYSCONFDIR)/icloud-for-linux/config.yaml.example
	install -Dm644 packaging/systemd/icloud-for-linux.service \
		$(DESTDIR)$(PREFIX)/lib/systemd/user/icloud-for-linux.service
	@echo "installed into $(DESTDIR)$(PREFIX)"

.PHONY: completions
completions: ## Generate the shell completions into dist/completions
	@mkdir -p $(DIST_DIR)/completions
	$(CARGO) build --release --features completions --bin $(BINARY)-completions
	$(TARGET_DIR)/release/$(BINARY)-completions bash > $(DIST_DIR)/completions/$(BINARY).bash
	$(TARGET_DIR)/release/$(BINARY)-completions zsh  > $(DIST_DIR)/completions/_$(BINARY)
	$(TARGET_DIR)/release/$(BINARY)-completions fish > $(DIST_DIR)/completions/$(BINARY).fish

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
ARCH ?= $(shell uname -m)

.PHONY: dist
dist: release ## Produce a release tarball in dist/
	@rm -rf $(DIST_DIR)/$(PKG)-$(VERSION)
	@mkdir -p $(DIST_DIR)/$(PKG)-$(VERSION)
	install -Dm755 $(TARGET_DIR)/release/$(BINARY) $(DIST_DIR)/$(PKG)-$(VERSION)/$(BINARY)
	install -Dm644 LICENSE $(DIST_DIR)/$(PKG)-$(VERSION)/LICENSE
	install -Dm644 README.md $(DIST_DIR)/$(PKG)-$(VERSION)/README.md
	install -Dm644 config.example.yaml $(DIST_DIR)/$(PKG)-$(VERSION)/config.example.yaml
	@tar -czf $(DIST_DIR)/$(PKG)-$(VERSION)-linux-$(ARCH).tar.gz -C $(DIST_DIR) $(PKG)-$(VERSION)
	@rm -rf $(DIST_DIR)/$(PKG)-$(VERSION)
	@echo "→ $(DIST_DIR)/$(PKG)-$(VERSION)-linux-$(ARCH).tar.gz"

.PHONY: dist-source
dist-source: ## Produce the source tarball the packages are built from
	@rm -rf $(DIST_DIR)/$(PKG)-$(VERSION) $(DIST_DIR)/$(PKG)-$(VERSION).tar.gz
	@mkdir -p $(DIST_DIR)/$(PKG)-$(VERSION)
	@tar -cf - --exclude=target --exclude=dist --exclude=.git \
		Cargo.toml Cargo.lock LICENSE NOTICE README.md CHANGELOG.md \
		CONTRIBUTING.md config.example.yaml rustfmt.toml \
		Makefile Dockerfile Dockerfile.dev docker-compose.yml \
		src tests docs packaging debian flake.nix nix \
		| tar -xf - -C $(DIST_DIR)/$(PKG)-$(VERSION)
	@tar -czf $(DIST_DIR)/$(PKG)-$(VERSION).tar.gz -C $(DIST_DIR) $(PKG)-$(VERSION)
	@rm -rf $(DIST_DIR)/$(PKG)-$(VERSION)
	@echo "→ $(DIST_DIR)/$(PKG)-$(VERSION).tar.gz"

.PHONY: package-rpm
package-rpm: dist-source ## Build the RPM (needs rpmbuild)
	@mkdir -p $(RPM_TOPDIR)/SOURCES $(RPM_TOPDIR)/SPECS $(RPM_TOPDIR)/BUILD \
		$(RPM_TOPDIR)/RPMS $(RPM_TOPDIR)/SRPMS
	cp $(DIST_DIR)/$(PKG)-$(VERSION).tar.gz $(RPM_TOPDIR)/SOURCES/
	cp packaging/rpm/$(PKG).spec $(RPM_TOPDIR)/SPECS/
	rpmbuild --define "_topdir $(RPM_TOPDIR)" --define "_dbpath $(RPM_TOPDIR)" \
		-bb $(RPM_TOPDIR)/SPECS/$(PKG).spec
	@mkdir -p $(DIST_DIR)/packages
	@cp $(RPM_TOPDIR)/RPMS/*/*.rpm $(DIST_DIR)/packages/
	@echo "→ $(DIST_DIR)/packages/"

.PHONY: package-deb
package-deb: ## Build the Debian package (needs dpkg-buildpackage)
	dpkg-buildpackage -us -uc -b
	@mkdir -p $(DIST_DIR)/packages
	@cp ../$(PKG)_$(VERSION)_*.deb $(DIST_DIR)/packages/
	@echo "→ $(DIST_DIR)/packages/"

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
