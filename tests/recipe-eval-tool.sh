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
# rust + C toolchain (human 2026-07-06), resolved by `td-builder provision-{rust,cc}`
# (builder/src/stage0.rs) — the SAME resolvers the cargo-test gate uses (a PROVIDED
# TD_RUST_HOME/TD_CC_HOME, or rustc+cargo / system cc on PATH, else rustup). The
# crate has NO [dependencies] so `--frozen` touches no network. No `guix shell`.
set -eu

base="${1:?usage: recipe-eval-tool.sh BASEDIR}"
root=$(cd "$(dirname "$0")/.." && pwd)
td="${TD_BUILDER_SELF:?recipe-eval-tool requires TD_BUILDER_SELF (gate-run exports it)}"

# Preserve provision-{rust,cc}'s exit code (69 = EX_UNAVAILABLE, no toolchain in
# the jail) so callers can degrade to Unprovisioned/tolerated instead of RED (#469).
rustpath=`"$td" provision-rust` || { rc=$?; echo "recipe-eval-tool: could not provision a rust toolchain (td-builder provision-rust)" >&2; exit $rc; }
ccpath=`"$td" provision-cc` || { rc=$?; echo "recipe-eval-tool: could not provision a C toolchain (td-builder provision-cc)" >&2; exit $rc; }

# STATICALLY link the evaluator for x86_64-unknown-linux-musl: `+crt-static` pulls
# in musl's self-contained libc.a and the bundled `rust-lld` links it with NO
# external cc/glibc. A static binary has an EMPTY runtime closure — no DT_NEEDED,
# no DT_RUNPATH — so it never loads libgcc_s.so.1/libc.so.6 from the MUTABLE
# ~/.guix-home/profile/lib that a glibc gcc ld-wrapper would otherwise bake in as
# a runpath and that vanishes while guix-home reconfigures or GCs ("error while
# loading shared libraries: libgcc_s.so.1", exit 127), flaking this control-plane
# tool and reddening the daily backstop. Fixing it at the SOURCE (crt-static musl)
# supersedes pinning a runpath (re #469). The recipes crate is dependency-free
# (pure std, no proc-macros).
#
# cargo reads exactly ONE rustflags source (first-set wins, no merge). We set the
# HIGHEST-precedence one, CARGO_ENCODED_RUSTFLAGS (one rustc arg per \037 field),
# because a guix cargo is a WRAPPER that re-injects RUSTFLAGS="… -C linker=<gcc>
# -rpath <guix-lib>" at RUNTIME: a plain RUSTFLAGS or a per-target
# CARGO_TARGET_<musl>_RUSTFLAGS would be outranked/overwritten by that, dropping
# rust-lld and baking a mutable guix-home DT_RUNPATH that fails assert-static
# below. CARGO_ENCODED_RUSTFLAGS is the one form the wrapper cannot touch. With
# `--target <musl>` set it applies to the musl binary ONLY; the HOST build script
# links via CARGO_TARGET_<host-triple>_LINKER = the provisioned cc.

# Pin the compiler and the HOST build-script linker to the provisioned toolchain
# (Codex P2): resolve rustc absolute to PIN it, and the gcc that links the host
# build script (`cc` may be absent by that name under a guix profile) so no
# inherited RUSTC / CARGO_TARGET_<host>_LINKER substitutes a different one.
cc=`PATH="$ccpath" command -v cc 2>/dev/null || PATH="$ccpath" command -v gcc 2>/dev/null` \
  || { echo "recipe-eval-tool: no cc/gcc in provisioned C toolchain ($ccpath)" >&2; exit 1; }
rustc=`PATH="$rustpath" command -v rustc 2>/dev/null` \
  || { echo "recipe-eval-tool: no rustc in provisioned rust toolchain ($rustpath)" >&2; exit 1; }
# The host triple cargo compiles the build script for; its per-target linker var
# gets $cc. cargo normalizes both `-` and `.` to `_` in the var name.
host=`"$rustc" -vV | sed -n 's/^host: //p'` \
  || { echo "recipe-eval-tool: \`rustc -vV\` failed" >&2; exit 1; }
[ -n "$host" ] || { echo "recipe-eval-tool: no host triple from \`rustc -vV\`" >&2; exit 1; }
hostvar=`printf '%s' "$host" | tr 'a-z.-' 'A-Z__'`
linkervar="CARGO_TARGET_${hostvar}_LINKER"

mkdir -p "$base/home" "$base/target"
# Build CARGO_ENCODED_RUSTFLAGS with \037 (0x1f) field separators — one rustc
# argument per field. PIN rustc (absolute) and set the wrappers to "" rather than
# unsetting them: an unset RUSTC/RUSTC_WRAPPER lets cargo read
# `build.rustc`/`build.rustc-wrapper` from a `.cargo/config.toml` walked up from the
# manifest, whereas an explicit value overrides config (Agy review, PR #534). ""
# means "no wrapper" to cargo. `env` sets the dynamically named per-host linker var.
encoded_rustflags=`printf '%s\037%s\037%s\037%s\037%s\037%s' \
  -C target-feature=+crt-static -C linker=rust-lld -C linker-flavor=ld.lld`
env \
  PATH="$rustpath:$ccpath:$PATH" \
  RUSTC="$rustc" RUSTC_WRAPPER="" RUSTC_WORKSPACE_WRAPPER="" \
  CARGO_ENCODED_RUSTFLAGS="$encoded_rustflags" \
  "$linkervar=$cc" \
  CARGO_HOME="$base/home" CARGO_TARGET_DIR="$base/target" \
  cargo build --release --frozen --target x86_64-unknown-linux-musl --manifest-path "$root/recipes/Cargo.toml" >"$base/build.log" 2>&1 \
  || { echo "recipe-eval-tool: cargo build failed:" >&2; tail -20 "$base/build.log" >&2; exit 1; }

bin="$base/target/x86_64-unknown-linux-musl/release/td-recipe-eval"
test -x "$bin" || { echo "recipe-eval-tool: no td-recipe-eval at $bin" >&2; exit 1; }

# Fail closed if the toolchain silently linked the evaluator dynamically: a
# dynamic control-plane binary drags a host runtime closure (and a mutable
# guix-home runpath) that the #469 sandbox boundary must deny.
"$td" assert-static "$bin" >/dev/null || { echo "recipe-eval-tool: td-recipe-eval is not statically linked" >&2; exit 1; }

printf '%s\n' "$bin" > "$base/recipe-eval-path"
echo "$bin"
