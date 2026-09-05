CARGO := cargo
DOCKER := docker
TARGET := x86_64-unknown-linux-musl
PROFILE := release

.PHONY: all build dev release test check lint fmt fmt-check clean doc run help
.PHONY: docker-build docker-run docker-compose-build docker-compose-up docker-compose-down docker-compose-logs docker-touch-mmdb
.PHONY: patch-geoip2-rs install

DEFAULT_TARGET := $(TARGET)
DEFAULT_PROFILE := $(PROFILE)

all: build

help:
	@echo "Usage: make [target]"
	@echo ""
	@echo "Build targets:"
	@echo "  build              Build release binary ($(TARGET))"
	@echo "  release            Alias for build"
	@echo "  run                Run in release mode"
	@echo ""
	@echo "Development targets:"
	@echo "  test               Run tests"
	@echo "  check              Check code without building"
	@echo "  lint               Run clippy with warnings as errors"
	@echo "  fmt                Format code"
	@echo "  fmt-check          Check code formatting"
	@echo "  doc                Generate documentation"
	@echo "  clean              Clean build artifacts"
	@echo ""
	@echo "Docker targets:"
	@echo "  docker-build       Build docker image"
	@echo "  docker-run         Build and run docker container"
	@echo "  docker-compose-build Build docker-compose services"
	@echo "  docker-compose-up  Start docker-compose services (detached)"
	@echo "  docker-compose-down Stop docker-compose services"
	@echo "  docker-compose-logs View docker-compose logs (follow)"
	@echo "  docker-touch-mmdb  Trigger mmdb reload in container"
	@echo ""
	@echo "Other targets:"
	@echo "  ci                 Run CI pipeline (fmt-check, lint, test, build)"
	@echo "  help               Show this help message"

patch-geoip2-rs:
	@mkdir -p vendor
	@if [ ! -d vendor/geoip2-rs/.git ]; then \
		rm -rf vendor/geoip2-rs; \
		git clone --depth 1 https://github.com/IncSW/geoip2-rs vendor/geoip2-rs; \
	fi
	@cd vendor/geoip2-rs && git apply ../../geoip2-rs.patch 2>/dev/null || true

run: patch-geoip2-rs
	$(CARGO) run --release --target $(TARGET)

build: patch-geoip2-rs
	$(CARGO) build --release --target $(TARGET)

release: build

test: patch-geoip2-rs
	$(CARGO) test --target $(TARGET)

check:
	$(CARGO) check --target $(TARGET)

lint: patch-geoip2-rs
	$(CARGO) clippy --target $(TARGET) --all-targets --all-features -- -D warnings

fmt:
	$(CARGO) fmt

fmt-check:
	$(CARGO) fmt -- --check

doc:
	$(CARGO) doc --no-deps

clean:
	$(CARGO) clean
	rm -Rf firehol-blocklist-ipsets firehol.mmdb vendor target

docker-build:
	$(DOCKER) build -t haproxy-spoa-ip-reputation-firehol .

docker-run: docker-build
	$(DOCKER) run --rm -p 9000:9000 -p 8405:8405 \
		-e RUST_LOG=info \
		-e SPOA_LISTEN_ADRESS=0.0.0.0:9000 \
		-e MMDB_PATH=/app/firehol.mmdb \
		-e DROP_CATEGORY=abuse \
		-v $(shell pwd)/firehol.mmdb:/app/firehol.mmdb:ro \
		haproxy-spoa-ip-reputation-firehol

docker-compose-build: patch-geoip2-rs
	$(DOCKER) compose build

docker-compose-up: docker-compose-build
	$(DOCKER) compose up

docker-compose-down:
	$(DOCKER) compose down

docker-compose-logs:
	$(DOCKER) compose logs -f --no-log-prefix --timestamps | sort

docker-touch-mmdb:
	$(DOCKER) compose exec spoa touch /app/firehol.mmdb

ci: fmt-check lint test build