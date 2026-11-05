CARGO ?= cargo
PYTHON ?= python3
TARGETS ?= --all-targets
FEATURES ?=
CARGO_FLAGS = --locked --workspace $(if $(FEATURES),--features $(FEATURES),)

.PHONY: all build check test fmt fmt-check clippy architecture verify clean
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
clean:
	$(CARGO) clean
