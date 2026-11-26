CARGO ?= cargo
PYTHON ?= python3
TARGETS ?= --all-targets
FEATURES ?=
CARGO_FLAGS = --locked --workspace $(if $(FEATURES),--features $(FEATURES),)

.PHONY: all build check test fmt fmt-check clippy architecture verify sandbox-verify drive-s3-verify clean
all: build
build:
	+$(CARGO) build $(CARGO_FLAGS)
check:
	+$(CARGO) check $(CARGO_FLAGS) $(TARGETS)
test:
	+$(CARGO) test $(CARGO_FLAGS)
fmt:
	$(CARGO) fmt --all
fmt-check:
	$(CARGO) fmt --all -- --check
clippy:
	+$(CARGO) clippy $(CARGO_FLAGS) $(TARGETS) -- -D warnings
architecture:
	$(PYTHON) tests/architecture.py
verify: fmt-check clippy test architecture
drive-s3-verify:
	+$(CARGO) build --locked -p agentdrived
	$(PYTHON) tests/drive_s3_smoke.py --daemon "$(or $(CARGO_TARGET_DIR),target)/debug/agentdrived$(if $(filter Windows_NT,$(OS)),.exe)"
sandbox-verify:
	$(PYTHON) tests/sandbox/check_sources.py
	+$(CARGO) check --locked --manifest-path sandbox/worker/Cargo.toml
	$(PYTHON) -m unittest discover -s tests/sandbox -p 'test_*.py'
clean:
	$(CARGO) clean
