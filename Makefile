# Sandfork developer targets. Run inside Linux (WSL2 on Windows), with the
# repository anywhere and build output under $HOME (see scripts/cargo.sh).

PY       ?= python3
VENV     ?= $(HOME)/.cache/sandfork/venv
PYTHON   := $(VENV)/bin/python
CARGO    := bash scripts/cargo.sh
TARGET   ?= $(HOME)/.cache/sandfork/target
SOCKET   ?= /tmp/sandfork-dev.sock
PROFILE  ?= debug

.PHONY: all build release rt venv proto-py test test-rust test-ns test-python test-local daemon lint tasks validate-tasks bench bench-t3 charts doctor clean

all: build

build:
	$(CARGO) build --workspace
	PROFILE=debug bash scripts/mkrt.sh

release:
	$(CARGO) build --workspace --release
	PROFILE=release bash scripts/mkrt.sh

venv:
	$(PY) -m venv $(VENV)
	$(VENV)/bin/pip install -q -e "python[dev]" matplotlib numpy pandas
	@echo "for the RL loop: $(VENV)/bin/pip install -e 'python[rl]' --extra-index-url https://download.pytorch.org/whl/cpu"

proto-py:
	PYTHON=$(PYTHON) bash scripts/gen-proto-py.sh

# Unit and property tests, plus the namespace integration tests (which skip
# themselves where user namespaces are unavailable).
test-rust:
	$(CARGO) test --workspace

test-ns: build
	SANDFORK_REQUIRE_NS=1 $(CARGO) test -p sandfork-spawner --test live
	SANDFORK_REQUIRE_NS=1 PROPTEST_CASES=200 $(CARGO) test -p sandfork-layers --test overlay_model

# Python unit tests, then the end-to-end tests against a throwaway daemon.
test-python: build
	cd python && $(PYTHON) -m pytest -q -p no:cacheprovider tests --ignore=tests/test_daemon.py
	bash scripts/with-daemon.sh $(SOCKET) sh -c 'cd python && SANDFORK_SOCKET=$(SOCKET) $(PYTHON) -m pytest -q -p no:cacheprovider tests/test_daemon.py'

test: test-rust test-ns test-python

# What a new contributor runs on WSL2: the same as CI's rootless job.
test-local: lint test

lint:
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	cd python && $(VENV)/bin/ruff check sandfork sandfork_rl sandfork_tasks tests ../bench

daemon: build
	bash scripts/dev-daemon.sh --socket $(SOCKET)

doctor:
	bash scripts/probe.sh

tasks:
	bash scripts/wheels.sh
	PYTHONPATH=python $(PYTHON) -m sandfork_tasks.generate --out tasks --per-family 200 --python $(PYTHON)

validate-tasks: build
	bash scripts/with-daemon.sh $(SOCKET) env PYTHONPATH=python SANDFORK_SOCKET=$(SOCKET) $(PYTHON) -m sandfork_tasks.validate_daemon --per-family 10

# Every measurement-only (T3) experiment, from one command, on a fresh machine.
bench-t3: release
	PROFILE=release bash scripts/with-daemon.sh $(SOCKET) env SANDFORK_SOCKET=$(SOCKET) $(PYTHON) bench/run.py e1 e2 e3 e6 --reps 5
	$(PYTHON) bench/e5_sim.py --seeds 3
	$(PYTHON) bench/charts.py

bench: bench-t3

charts:
	$(PYTHON) bench/charts.py

clean:
	$(CARGO) clean
