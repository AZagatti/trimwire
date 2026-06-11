# trimwire project tasks. Targets are thin convenience wrappers around
# cargo + pytest commands. See DEVELOPMENT.md for the build plan.

.PHONY: help check fmt clippy test phase0 phase0-dump phase0-verify build release clean

help:
	@echo "Available targets:"
	@echo "  check       cargo check + clippy + fmt-check"
	@echo "  fmt         cargo fmt --all"
	@echo "  clippy      cargo clippy --all-targets -- -D warnings"
	@echo "  test        cargo test --all-features"
	@echo "  phase0      Python test harness (tests/phase0/)"
	@echo "  phase0-dump regenerate the committed parity fixtures (tests/fixtures/expected/)"
	@echo "  phase0-verify  fail if the committed parity fixtures are stale (CI gate)"
	@echo "  build       cargo build --release"
	@echo "  clean       cargo clean"

check: fmt-check clippy test

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --all-targets -- -D warnings

test:
	cargo test --all-features --no-fail-fast

PHASE0_VENV := tests/phase0/.venv
PHASE0_PY   := $(PHASE0_VENV)/bin/python3

$(PHASE0_PY):
	@command -v uv >/dev/null 2>&1 || (echo "uv not on PATH. Install: https://docs.astral.sh/uv/" && exit 1)
	uv venv $(PHASE0_VENV) --python 3.12 --quiet
	uv pip install --python $(PHASE0_PY) --quiet -r tests/phase0/requirements.txt

phase0: $(PHASE0_PY)
	cd tests/phase0 && PYTHONPATH=. ../../$(PHASE0_PY) -m pytest -v

# Regenerate the Python-reference parity fixtures the Rust tests diff against.
phase0-dump: $(PHASE0_PY)
	PYTHONPATH=tests/phase0 $(PHASE0_PY) tests/phase0/dump_expected.py

# CI gate: regenerate the fixtures and fail if the committed copies are stale
# (i.e. the Python reference changed but the snapshots weren't refreshed). The
# Rust parity tests diff against these files, so a stale snapshot would let a
# reference divergence pass unnoticed.
phase0-verify: phase0-dump
	git diff --exit-code -- tests/fixtures/expected \
		|| (echo "::error::tests/fixtures/expected is stale — run 'make phase0-dump' and commit" && exit 1)

build:
	cargo build --release

clean:
	cargo clean
