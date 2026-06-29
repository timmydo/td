#!/bin/sh
# tests/recipe-eval-tool.sh BASEDIR — build td's OWN recipe/spec evaluator
# (the dependency-free `td-recipe` crate, recipes/) and print the binary path.
#
# This REPLACES the boa td-recipe-eval (tests/ts-eval-tool.sh) on the build path: the
# package surface is declared in Rust now (recipes/src/catalog.rs +
# recipes/src/specs.rs), so the loop evaluates recipes/specs with `td-recipe-eval`
# instead of transpiling+evaluating `.ts` through tsgo+boa. Built ONCE by the
# build-recipes prelude into BASEDIR (content cached via CARGO_TARGET_DIR), then
# read by cache-lib's `load_recipe_eval` and by every build gate via ts-emit.sh.
#
# Offline + toolchain-only (the cargo-test pattern): `guix shell --no-substitutes
# --no-offload rust rust:cargo gcc-toolchain` resolves the warm rust toolchain;
# the crate has NO [dependencies] so `--frozen` touches no network. The rust
# toolchain is the guix-built SEED (§5, retired last) — same status as the C
# toolchain; this is `guix shell` provisioning, NOT a `guix build -e (system M) PKG`
# packager site.
set -eu

base="${1:?usage: recipe-eval-tool.sh BASEDIR}"
root=$(cd "$(dirname "$0")/.." && pwd)

mkdir -p "$base/home" "$base/target"
GUIX="${TD_GUIX:-guix}"
CARGO_HOME="$base/home" CARGO_TARGET_DIR="$base/target" \
  $GUIX shell --no-substitutes --no-offload rust "rust:cargo" gcc-toolchain -- \
  cargo build --release --frozen --manifest-path "$root/recipes/Cargo.toml" >"$base/build.log" 2>&1 \
  || { echo "recipe-eval-tool: cargo build failed:" >&2; tail -20 "$base/build.log" >&2; exit 1; }

bin="$base/target/release/td-recipe-eval"
test -x "$bin" || { echo "recipe-eval-tool: no td-recipe-eval at $bin" >&2; exit 1; }

printf '%s\n' "$bin" > "$base/recipe-eval-path"
echo "$bin"
