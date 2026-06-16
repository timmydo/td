# cargo-test — td-builder's Rust unit tests run DIRECTLY via `cargo test`
# (offline, toolchain-only), the loop-latency brainstorm's "push logic down into
# fast unit tests" first step. The 39 `#[test]`s in builder/src/*.rs (NAR framing,
# SHA-256 vectors, drv parse/emit, the store-db SQLite encode/decode + reader,
# scan, sandbox) today run ONLY inside the cargo-build-system package build — a
# full release rebuild that ~15 heavy gates trigger. This gate exercises the SAME
# tests in seconds via a direct `cargo test`, so a Rust-logic regression reds here
# (sub-20s) instead of only deep in the td-builder/store/drv ladder. Additive: it
# adds a test run, removes/loosens nothing (directive 3).
#
# Offline by construction: `guix shell --no-substitutes --no-offload` resolves
# rust + cargo + a cc (gcc-toolchain, rust's default linker) from the WARM store
# (rust is already in td-builder's build closure), and `cargo test --frozen`
# (= --locked --offline) on a DEPENDENCY-FREE crate (builder/Cargo.toml has no
# [dependencies]) touches no network. Scratch CARGO_HOME/CARGO_TARGET_DIR live in
# .cargo-test-scratch/ at the repo ROOT — OUTSIDE builder/, so they cannot perturb
# the td-builder package source hash (system/td-builder.scm local-file "../builder").
# pipefail keeps a FAILED `cargo test` from being greened by the `tee`, and the
# `test result: ok. <N> passed` (N>=1) assertion rejects a vacuous 0-test run.
#
# Scope: builder/ only — ts-eval has no #[test]s and vendors boa, out of scope.
HEAVY_GATES += cargo-test
# Not FAST_GATES: cargo test needs the rust toolchain, which the small td-ci-fast
# image does NOT carry (ci/lower-fast-drvs.sh ships node+tsc+cheap-rung closures
# only), so tagging it FAST would red the required offline `check-fast`. It runs
# in the dev-machine full ./check.sh (the §7.2 step-2 landing gate) and the full
# td-ci validate job — both carry rust. Promote to FAST later by adding the
# rust+builder closure to ci/lower-fast-drvs.sh (grows the fast image).
cargo-test:
	@echo ">> cargo-test: td-builder Rust unit tests via cargo test (offline, toolchain-only)"
	@set -euo pipefail; \
	scratch="$(CURDIR)/.cargo-test-scratch"; \
	rm -rf "$$scratch"; mkdir -p "$$scratch/home" "$$scratch/target"; \
	log="$$scratch/out.log"; \
	CARGO_HOME="$$scratch/home" CARGO_TARGET_DIR="$$scratch/target" \
	  $(GUIX) shell --no-substitutes --no-offload rust "rust:cargo" gcc-toolchain -- \
	  cargo test --frozen --manifest-path builder/Cargo.toml 2>&1 | tee "$$log"; \
	grep -qE 'test result: ok\. [1-9][0-9]* passed' "$$log" || \
	  { echo "ERROR: cargo test reported no passing tests (vacuous run?)" >&2; exit 1; }; \
	rm -rf "$$scratch"
