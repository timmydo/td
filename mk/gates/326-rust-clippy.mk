# rust-clippy — the dependency-free engine crates (builder + recipes) lint clean
# under the td Rust coding rules (AGENTS.md → "Rust code"): NO panicking surface
# (`unwrap`/`expect`/`panic!`/`unreachable!`/`todo!`/`unimplemented!`), `.get(i)`
# over panicking `xs[i]` indexing, and `unsafe` confined to the raw-syscall layer.
# The rules are declared as a `[lints]` table in each crate's Cargo.toml at `deny`;
# existing code that pre-dates the rules is grandfathered per-file (`#![allow(...)]`
# in module files) or per-item (`#[allow(...)]` on the crate-root's own fns/impls,
# because a crate-root inner `#![allow]` would be crate-GLOBAL and silently exempt
# everything). So a denied lint reds ONLY on NEW code — a fresh module or a fresh
# top-level item — which is exactly the "enforce for new code" contract.
#
# Also the one-way "no crates" guard for the engine: builder + recipes MUST stay
# dependency-free (Cargo.lock = 1 package each — the crate itself). The network
# tools (fetch/feed/subst) carry the vendored FSDG crates and are NOT linted here
# (they cannot compile offline without their vendored closure); their Cargo.toml
# still declares the same `[lints]` table so a local `cargo clippy` enforces it.
#
# Offline by construction, like cargo-test: `guix shell --no-substitutes
# --no-offload` resolves rust (which carries cargo-clippy) + a cc from the WARM
# store, and `cargo clippy --frozen` on a DEPENDENCY-FREE crate touches no network.
# Scratch CARGO_HOME/target live at the repo root, OUTSIDE the crate dirs, so they
# cannot perturb the td-builder/td-recipe package source hashes. The gate reds on a
# denied lint via clippy's nonzero exit (the `[lints]` deny level = a hard error).
HEAVY_GATES += rust-clippy
# Part of the build-engine smoke tier (`check-engine`): a Rust-rules regression on
# an engine change reds here in the ~2-min smoke, not only deep in a heavy build.
ENGINE_GATES += rust-clippy
# Not FAST_GATES: cargo clippy needs the rust toolchain, absent from the small
# td-ci-fast image (same reason as cargo-test).
rust-clippy:
	@echo ">> rust-clippy: engine crates (builder + recipes) lint clean under the td Rust rules (no panic surface, .get over indexing, unsafe confined) — offline, toolchain-only"
	@set -euo pipefail; \
	for crate in builder recipes; do \
	  n=`grep -c '^\[\[package\]\]' "$$crate/Cargo.lock"`; \
	  test "$$n" -eq 1 || { echo "ERROR: $$crate is no longer dependency-free (Cargo.lock lists $$n packages; the engine must carry ZERO crates — see AGENTS.md 'Rust code')" >&2; exit 1; }; \
	done; \
	scratch="$(CURDIR)/.rust-clippy-scratch"; \
	rm -rf "$$scratch"; mkdir -p "$$scratch/home" "$$scratch/target"; \
	CARGO_HOME="$$scratch/home" CARGO_TARGET_DIR="$$scratch/target" \
	  $(GUIX) shell --no-substitutes --no-offload rust "rust:cargo" gcc-toolchain -- \
	  sh -c 'cargo clippy --frozen --manifest-path builder/Cargo.toml && \
	         cargo clippy --frozen --manifest-path recipes/Cargo.toml'; \
	rm -rf "$$scratch"; \
	echo "PASS: rust-clippy — builder + recipes are dependency-free and lint clean under the td Rust rules."
