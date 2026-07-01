# cargo-test — the td Rust engine's fast checks: `cargo clippy` (the coding-rules
# lint) THEN `cargo test` (the unit tests), run DIRECTLY on the dependency-free
# engine crates (offline, toolchain-only). The loop-latency brainstorm's "push
# logic down into fast unit tests" first step, now also the enforcement point for
# the AGENTS.md Rust coding rules.
#
# clippy leg (AGENTS.md → "Rust code"): builder + recipes must lint clean under the
# `[lints]` table each Cargo.toml declares at `deny` — NO panicking surface
# (unwrap/expect/panic!/unreachable!/todo!/unimplemented!), `.get(i)` over
# panicking `xs[i]`, and `unsafe` confined to the raw-syscall layer. Existing code
# is grandfathered (per-file `#![allow]` in modules; per-item `#[allow]` on the
# crate root's own fns/impls — a crate-root inner `#![allow]` is crate-GLOBAL and
# would silently exempt everything), so a denied lint reds ONLY on NEW code. Also
# the one-way "no crates" guard for the engine: builder + recipes stay
# dependency-free (Cargo.lock = 1 package each). The network tools
# (fetch/feed/subst) carry the vendored FSDG crates and can't compile offline, so
# they are NOT linted here; their Cargo.toml still declares the same `[lints]` table
# so a local `cargo clippy` enforces it.
#
# test leg: the `#[test]`s in builder/src/*.rs (NAR framing, SHA-256 vectors, drv
# parse/emit, the store-db SQLite encode/decode + reader, scan, sandbox) otherwise
# run ONLY inside the cargo-build-system package build — a full release rebuild that
# ~15 heavy gates trigger. Running them here reds a Rust-logic regression in seconds
# instead of deep in the td-builder/store/drv ladder.
#
# This gate INTENTIONALLY reuses one `guix shell` for clippy + test so it adds NO
# new guix-surface site (the folded clippy leg does not grow the shrink ratchet —
# tests/guix-surface-shrink.expected line for this file stays a single `shell`).
#
# Offline by construction: `guix shell --no-substitutes --no-offload` resolves rust
# (which carries cargo-clippy) + cargo + a cc (gcc-toolchain, rust's default linker)
# from the WARM store (rust is already in td-builder's build closure), and `cargo
# clippy/test --frozen` (= --locked --offline) on DEPENDENCY-FREE crates touches no
# network. Scratch CARGO_HOME/CARGO_TARGET_DIR live in .cargo-test-scratch/ at the
# repo ROOT — OUTSIDE the crate dirs, so they cannot perturb the td-builder/td-recipe
# package source hashes. `set -e` inside the shell + pipefail keep a FAILED clippy
# or test from being greened by the `tee`, and the `test result: ok. <N> passed`
# (N>=1) assertion rejects a vacuous 0-test run.
HEAVY_GATES += cargo-test
# The build-engine smoke tier (`check-engine`) is JUST this — compile the engine,
# lint it, and run its unit tests, ~2-4 min, no from-source builds. Anything that
# builds a package (bootstrap-build/build-plan/td-check/corpus/…) is NOT smoke; it
# stays in the full `check` / daily backstop.
ENGINE_GATES += cargo-test
# Not FAST_GATES: cargo clippy/test needs the rust toolchain, which the small
# td-ci-fast image does NOT carry (ci/lower-fast-drvs.sh ships node+tsc+cheap-rung
# closures only), so tagging it FAST would red the required offline `check-fast`. It
# runs in the dev-machine full ./check.sh (the §7.2 step-2 landing gate) and the
# full td-ci validate job — both carry rust. Promote to FAST later by adding the
# rust+builder closure to ci/lower-fast-drvs.sh (grows the fast image).
cargo-test:
	@echo ">> cargo-test: engine crates lint clean (cargo clippy: no panic surface, .get over indexing, unsafe confined) + td-builder unit tests (cargo test) — offline, toolchain-only"
	@set -euo pipefail; \
	for crate in builder recipes; do \
	  n=`grep -c '^\[\[package\]\]' "$$crate/Cargo.lock"`; \
	  test "$$n" -eq 1 || { echo "ERROR: $$crate is no longer dependency-free (Cargo.lock lists $$n packages; the engine must carry ZERO crates — AGENTS.md 'Rust code')" >&2; exit 1; }; \
	done; \
	scratch="$(CURDIR)/.cargo-test-scratch"; \
	rm -rf "$$scratch"; mkdir -p "$$scratch/home" "$$scratch/target"; \
	log="$$scratch/out.log"; \
	CARGO_HOME="$$scratch/home" CARGO_TARGET_DIR="$$scratch/target" \
	  $(GUIX) shell --no-substitutes --no-offload rust "rust:cargo" gcc-toolchain -- \
	  sh -c 'set -e; \
	    cargo clippy --frozen --manifest-path builder/Cargo.toml; \
	    cargo clippy --frozen --manifest-path recipes/Cargo.toml; \
	    cargo test  --frozen --manifest-path builder/Cargo.toml' 2>&1 | tee "$$log"; \
	grep -qE 'test result: ok\. [1-9][0-9]* passed' "$$log" || \
	  { echo "ERROR: cargo test reported no passing tests (vacuous run?)" >&2; exit 1; }; \
	rm -rf "$$scratch"; \
	echo "PASS: cargo-test — builder + recipes are dependency-free and lint clean; td-builder unit tests pass."
