# recipe-rs — td's package + system-spec surface declared in RUST (rust-recipe-surface
# track; the §5 move-off-Guile goal). boa/TypeScript/tsgo are RETIRED: recipes live in
# recipes/src/catalog.rs and specs in recipes/src/specs.rs, evaluated by td-recipe-eval.
# This gate compiles the dependency-free `td-recipe` crate OFFLINE, runs its unit tests,
# then asserts (tests/recipe-rs.sh) the surface is self-consistent — every recipe + spec
# emits valid round-tripping JSON, the guix-dependence manifest (tests/recipes-meta.json)
# is in sync, and `verify` discriminates a mismatch (negative control). Correctness vs
# upstream is the corpus differential's job (corpus-no-guix builds each recipe NAR-equal
# to guix), not a boa oracle.
#
# Offline by construction (the cargo-test pattern): `guix shell --no-substitutes
# --no-offload rust rust:cargo gcc-toolchain` resolves the warm rust toolchain; the crate
# has NO [dependencies] so `--frozen` touches no network. No guix package is built via
# `guix build -e (system M) PKG` — so NOTHING is added to the guix-surface ratchet.
HEAVY_GATES += recipe-rs
# Not FAST_GATES: needs the rust toolchain (absent from the small td-ci-fast image), same
# rationale as cargo-test.
recipe-rs:
	@echo ">> recipe-rs: the Rust package + spec surface (td-recipe crate) is self-consistent + the census manifest is in sync (rust-recipe-surface)"
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
	TD_RECIPE_EVAL="$$bin" sh tests/recipe-rs.sh; \
	rm -rf "$$scratch"
