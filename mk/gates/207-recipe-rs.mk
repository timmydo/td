# recipe-rs — td's package-recipe surface declared in RUST (rust-recipe-surface
# track; the §5 move-off-Guile goal, next step after the TS front-end). Compiles
# the dependency-free `td-recipe` crate (recipes/) OFFLINE, runs its unit tests,
# then asserts (tests/recipe-rs.sh) that the Rust catalog is equivalent to the
# boa/TypeScript surface it replaces: 1:1 coverage with tests/ts/recipe-*.ts,
# every recipe emits valid self-consistent JSON, `verify` discriminates a
# mismatch (negative control) — all DURABLE — plus the REMOVABLE migration oracle
# (boa's evaluation of each .ts canon-equals the Rust recipe; deleted when boa is
# retired, not the gate). boa stays the oracle here; no consumer cutover yet (the
# corpus still builds from boa JSON — cutover is a tracked follow-up,
# plan/rust-migration.md).
#
# Offline by construction (the cargo-test pattern): `guix shell --no-substitutes
# --no-offload` resolves rust+cargo+gcc-toolchain from the WARM store, and the
# crate has NO [dependencies] so `--frozen` touches no network. Scratch
# CARGO_HOME/CARGO_TARGET_DIR live OUTSIDE recipes/ so they cannot perturb the
# crate source hash. The oracle leg resolves boa (td-ts-eval) + native tsc (tsgo)
# exactly as the ts-diff gate does (warm cache hit). Heavy (a Rust build + boa
# closure + 53 tsc/boa evals), so it slots in the heavy pool near the TS gates.
HEAVY_GATES += recipe-rs
# Not FAST_GATES: needs the rust toolchain AND the boa closure, neither of which
# the small td-ci-fast image carries (same rationale as cargo-test / ts-diff).
recipe-rs:
	@echo ">> recipe-rs: the Rust package surface (td-recipe crate) is equivalent to the boa/TS surface (rust-recipe-surface)"
	@set -euo pipefail; \
	scratch="$(CURDIR)/.recipe-rs-scratch"; \
	rm -rf "$$scratch"; mkdir -p "$$scratch/home" "$$scratch/target"; \
	echo ">> build + unit-test the dependency-free td-recipe crate (offline, toolchain-only)"; \
	CARGO_HOME="$$scratch/home" CARGO_TARGET_DIR="$$scratch/target" \
	  $(GUIX) shell --no-substitutes --no-offload rust "rust:cargo" gcc-toolchain -- \
	  sh -c 'cargo test --frozen --manifest-path recipes/Cargo.toml \
	     && cargo build --release --frozen --manifest-path recipes/Cargo.toml' 2>&1 | tail -20; \
	bin="$$scratch/target/release/td-recipe-eval"; \
	test -x "$$bin" || { echo "ERROR: td-recipe-eval was not built at $$bin" >&2; exit 1; }; \
	echo ">> resolve native tsc (tsgo) + boa (td-ts-eval) for the migration-oracle leg"; \
	tsgo=`sh tests/tsgo.sh`; \
	boa=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-ts-eval)'`/bin/td-ts-eval; \
	test -n "$$tsgo" -a -x "$$tsgo/lib/tsc" -a -x "$$boa" || { echo "ERROR: could not resolve td-tsgo / td-ts-eval" >&2; exit 1; }; \
	TD_RECIPE_EVAL="$$bin" TD_TSGO="$$tsgo" TD_TS_EVAL="$$boa" TD_TSDIR="$(CURDIR)/tests/ts" \
	  sh tests/recipe-rs.sh; \
	rm -rf "$$scratch"
