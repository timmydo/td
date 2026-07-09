#!/bin/sh
# tests/recipe-eval-tool.sh BASEDIR — build td's OWN recipe/spec evaluator
# (the dependency-free `td-recipe` crate, recipes/) and print the binary path.
#
# This REPLACES the boa td-recipe-eval (tests/ts-eval-tool.sh) on the build path: the
# package surface is declared in Rust now (recipes/src/recipes/<stem>.rs, one
# self-registering file per recipe), so the loop evaluates recipes with `td-recipe-eval`
# instead of transpiling+evaluating `.ts` through tsgo+boa. Built ONCE by the
# build-recipes prelude into BASEDIR (content cached via CARGO_TARGET_DIR), then
# read by cache-lib's `load_recipe_eval` and invoked directly by build gates.
#
# Offline + toolchain-only, GUIX-FREE (the cargo-test pattern): the HOST brings the
# rust + C toolchain (human 2026-07-06), resolved by tools/provision-rust.sh +
# tools/provision-cc.sh — the SAME resolvers the cargo-test gate uses (a PROVIDED
# TD_RUST_HOME/TD_CC_HOME, or rustup/system cc, else the pinned lock seed). The
# crate has NO [dependencies] so `--frozen` touches no network. No `guix shell`.
set -eu

base="${1:?usage: recipe-eval-tool.sh BASEDIR}"
root=$(cd "$(dirname "$0")/.." && pwd)

rustpath=`sh "$root/tools/provision-rust.sh"` || { echo "recipe-eval-tool: could not provision a rust toolchain (tools/provision-rust.sh)" >&2; exit 1; }
ccpath=`sh "$root/tools/provision-cc.sh"` || { echo "recipe-eval-tool: could not provision a C toolchain (tools/provision-cc.sh)" >&2; exit 1; }

mkdir -p "$base/home" "$base/target"
PATH="$rustpath:$ccpath:$PATH" \
CARGO_HOME="$base/home" CARGO_TARGET_DIR="$base/target" \
  cargo build --release --frozen --manifest-path "$root/recipes/Cargo.toml" >"$base/build.log" 2>&1 \
  || { echo "recipe-eval-tool: cargo build failed:" >&2; tail -20 "$base/build.log" >&2; exit 1; }

bin="$base/target/release/td-recipe-eval"
test -x "$bin" || { echo "recipe-eval-tool: no td-recipe-eval at $bin" >&2; exit 1; }

printf '%s\n' "$bin" > "$base/recipe-eval-path"
echo "$bin"
