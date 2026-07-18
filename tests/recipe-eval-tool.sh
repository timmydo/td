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
# TD_RUST_HOME/TD_CC_HOME, or rustup/system cc, else the pinned lock seed). The
# crate has NO [dependencies] so `--frozen` touches no network. No `guix shell`.
set -eu

base="${1:?usage: recipe-eval-tool.sh BASEDIR}"
root=$(cd "$(dirname "$0")/.." && pwd)
td="${TD_BUILDER_SELF:?recipe-eval-tool requires TD_BUILDER_SELF (gate-run exports it)}"

rustpath=`"$td" provision-rust` || { echo "recipe-eval-tool: could not provision a rust toolchain (td-builder provision-rust)" >&2; exit 1; }
ccpath=`"$td" provision-cc` || { echo "recipe-eval-tool: could not provision a C toolchain (td-builder provision-cc)" >&2; exit 1; }

# STATICALLY link the evaluator (and its cargo build-script binaries) against a
# MATCHED static glibc. A static binary has an EMPTY runtime closure — no
# DT_NEEDED, no DT_RUNPATH — so it never loads libgcc_s.so.1/libc.so.6 from the
# MUTABLE ~/.guix-home/profile/lib that guix's gcc ld-wrapper would otherwise
# bake in as a runpath and that vanishes while guix-home reconfigures or GCs
# ("error while loading shared libraries: libgcc_s.so.1", exit 127), flaking
# this control-plane tool and reddening the daily backstop. Fixing it at the
# SOURCE (crt-static) supersedes pinning a runpath (re #469). The recipes crate
# is dependency-free (pure std, no proc-macros) so the flags apply cleanly.
# provision-glibc-static rejects a whitespace $gstatic, so the double-quoted
# `-L $gstatic` never splits.
#
# cargo reads exactly ONE rustflags source (first-set wins, no merge), and
# CARGO_ENCODED_RUSTFLAGS outranks the RUSTFLAGS we set below. Unset any ambient
# one on the build host so our static flags cannot be silently dropped (which
# would relink dynamic — caught by assert-static below, but a spurious
# env-dependent red, the failure mode this test removes).
gstatic=`"$td" provision-glibc-static` || { echo "recipe-eval-tool: no matched static glibc (td-builder provision-glibc-static) — set TD_GLIBC_STATIC_HOME, install a build-essential cc, or pin one in the lock" >&2; exit 1; }

# Pin the compiler and linker to the provisioned toolchain (Codex P2): resolve the
# gcc that owns the matched static glibc and pass it as `-C linker=` so an inherited
# CARGO_TARGET_<triple>_LINKER cannot pair the static glibc's crt objects with a
# mismatched driver (links clean, SIGSEGVs at startup), and resolve the provisioned
# rustc to an absolute path to PIN it.
cc=`PATH="$ccpath" command -v cc 2>/dev/null || PATH="$ccpath" command -v gcc 2>/dev/null` \
  || { echo "recipe-eval-tool: no cc/gcc in provisioned C toolchain ($ccpath)" >&2; exit 1; }
rustc=`PATH="$rustpath" command -v rustc 2>/dev/null` \
  || { echo "recipe-eval-tool: no rustc in provisioned rust toolchain ($rustpath)" >&2; exit 1; }
# `-C linker=$cc` rides in the space-split RUSTFLAGS below, so a whitespace cc path
# (only reachable via a whitespace TD_CC_HOME) would split the argument. Fail closed
# rather than mis-link, mirroring provision-glibc-static's guard on $gstatic (Codex
# review, PR #534). The Rust recipe_rs gate uses the \x1f-encoded form and is immune.
case "$cc" in
  *[[:space:]]*) echo "recipe-eval-tool: cc path '$cc' contains whitespace — move the C toolchain to a whitespace-free path (TD_CC_HOME)" >&2; exit 1 ;;
esac

mkdir -p "$base/home" "$base/target"
# Unset the tier-1 rustflags var so our RUSTFLAGS wins. PIN rustc (absolute) and
# set the wrappers to "" rather than unsetting them: an unset RUSTC/RUSTC_WRAPPER
# lets cargo read `build.rustc`/`build.rustc-wrapper` from a `.cargo/config.toml`
# walked up from the manifest, whereas an explicit value overrides config (Agy
# review, PR #534). "" means "no wrapper" to cargo.
unset CARGO_ENCODED_RUSTFLAGS
PATH="$rustpath:$ccpath:$PATH" \
RUSTC="$rustc" RUSTC_WRAPPER="" RUSTC_WORKSPACE_WRAPPER="" \
RUSTFLAGS="-C target-feature=+crt-static -C relocation-model=static -L $gstatic -C linker=$cc" \
CARGO_HOME="$base/home" CARGO_TARGET_DIR="$base/target" \
  cargo build --release --frozen --manifest-path "$root/recipes/Cargo.toml" >"$base/build.log" 2>&1 \
  || { echo "recipe-eval-tool: cargo build failed:" >&2; tail -20 "$base/build.log" >&2; exit 1; }

bin="$base/target/release/td-recipe-eval"
test -x "$bin" || { echo "recipe-eval-tool: no td-recipe-eval at $bin" >&2; exit 1; }

# Fail closed if the toolchain silently linked the evaluator dynamically: a
# dynamic control-plane binary drags a host runtime closure (and a mutable
# guix-home runpath) that the #469 sandbox boundary must deny.
"$td" assert-static "$bin" >/dev/null || { echo "recipe-eval-tool: td-recipe-eval is not statically linked" >&2; exit 1; }

printf '%s\n' "$bin" > "$base/recipe-eval-path"
echo "$bin"
