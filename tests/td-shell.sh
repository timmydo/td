#!/bin/sh
# tests/td-shell.sh — `td-builder shell` is td's own `guix shell`, with NO guix.
#
# `td shell PKG -- CMD` resolves PKG to a td RECIPE and BUILDS it with td-builder
# itself (the recipe → `td-builder build-recipe`, whose content-addressed cache
# makes this build-on-demand + cached), composes CMD's PATH from the td store
# OUTPUT, and execs. There is no `guix` process in the resolve/build/exec path; an
# unknown package errors, it does NOT fall back to guix. This gate proves it by
# running `td shell` with guix/Guile SCRUBBED FROM PATH — if it tried to call guix
# it could not. The package that lands on PATH is td's OWN build at td's OWN store
# path (distinct from guix's). North-Star step 1 (CLAUDE.md): td shell runs
# guix-free; the build still links the pinned toolchain SEED from the lock
# (guix-built today, the frozen seed tarball next — step 2).
#
# Tools (all td-built / guix-free): stage0 td-builder (cache-lib load_stage0) +
# td-recipe-eval (the Rust recipe catalog evaluator, load_recipe_eval, from the
# build-recipes prelude — `td shell` resolves PKG through it, NOT the deleted .ts
# surface). Realizing hello's pinned SEED closure up front is bare
# `guix build` of the lock's store paths (test setup, not a packager form, not in
# td shell's path) — the same warming every build gate does.
#
# Legs:
#   A [DURABLE behavioral] `td shell hello -- hello` greets, guix/Guile off PATH
#   B [DURABLE td-built]    the hello on PATH is td's OWN build at a td store path
#                           (under the cache, NOT guix's p3b2… path), runnable
#   C [DURABLE load-bearing] an unknown package errors ("no td recipe"), no guix fallback
#   D [REMOVABLE oracle]    td's hello is a DISTINCT store path from `guix build hello`
#                           (own, then diverge), same greeting — the guix differential,
#                           deleted when guix retires; A–C remain
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }

. tests/cache-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/td-shell"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
load_recipe_eval || fail "no td-built td-recipe-eval (the build-recipes prelude must run first)"
test -x "$TD_RECIPE_EVAL" || fail "td recipe evaluator not executable"
echo ">> td tools (guix-free): stage0=$TB  recipe-eval=$TD_RECIPE_EVAL"

# A scrubbed PATH for the td shell process: coreutils + bash from hello's pinned
# seed, NO guix/Guile — so a green run PROVES td shell uses no guix process.
# coreutils + bash are DECLARED gate inputs (#353): resolved by the runner.
cu=${TD_GATE_INPUT_COREUTILS:-}
sh_=${TD_GATE_INPUT_BASH:-}
test -n "$cu" -a -n "$sh_" || { echo "ERROR: TD_GATE_INPUT_{COREUTILS,BASH} unset — run via td-builder gate-run, which resolves the gate's declared inputs" >&2; exit 1; }
test -n "$cu" -a -n "$sh_" || fail "no coreutils/bash in tests/hello-no-guix.lock"
if ls "$cu/bin" "$sh_/bin" | grep -qE '^(guix|guile)$'; then fail "guix/guile on the scrubbed PATH"; fi
SCRUB="$cu/bin:$sh_/bin"

# Warm hello's pinned seed closure offline (test setup; bare `guix build` of the
# lock's store paths — NOT a packager form, NOT in td shell's path).
grep ' /gnu/store/' tests/hello-no-guix.lock | sed 's/^[^ ]* //' | sort -u | xargs guix build >/dev/null \
  || fail "could not realize hello's pinned seed closure"

cache="`pwd`/.td-build-cache/td-shell-pkgs"; rm -rf "$cache"; mkdir -p "$cache/tmp"

# td shell, run with guix/Guile OFF PATH (env -i + scrubbed PATH ⇒ no guix process).
# TD_SHELL_STORE_DB is the SEED STORE DIR the seed-package build CONTENT-SCANS for hello's
# input closure (== guix gc -R; gate 290) — the live /gnu/store (the guix-BUILT toolchain
# seed, retired last by step 2, gate td-shell-seed), NOT guix's private /var/guix/db. No
# guix store DB is read. (A DB *file* here is a no-op: the scan needs the store DIR so the
# transitive closure — e.g. bash's libreadline — is staged, not just the lock's direct roots.)
tdshell() {
  env -i HOME="$cache" TMPDIR="$cache/tmp" PATH="$SCRUB" \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    TD_RECIPE_EVAL="$TD_RECIPE_EVAL" \
    TD_SHELL_LOCKS=tests TD_SHELL_STORE_DB=/gnu/store \
    TD_SHELL_CACHE="$cache" \
    "$TB" shell "$@"
}

# --- Leg A: DURABLE behavioral (build + run td's hello, no guix process) -------
echo ">> [DURABLE behavioral] td shell hello -- hello (guix/Guile OFF PATH)"
out=`tdshell hello -- hello 2>"$cache/a.err"` \
  || { tail -20 "$cache/a.err" >&2; fail "td shell hello -- hello exited nonzero"; }
test "$out" = "Hello, world!" || fail "td shell hello -- hello printed '$out' (expected 'Hello, world!')"
echo "   ok: td built its own hello (no guix on PATH) and it greeted"

# --- Leg B: DURABLE td-built (the hello on PATH is td's OWN build) -------------
echo ">> [DURABLE td-built] the hello on PATH is td's own build at a td store path"
hb=`tdshell hello -- bash -c 'command -v hello'` || fail "could not locate hello on the composed PATH"
case "$hb" in
  "$cache"/hello/newstore/*-hello-*/bin/hello) : ;;
  *) fail "hello on PATH is '$hb' — not a td-built path under $cache" ;;
esac
test -x "$hb" || fail "the td hello ($hb) is not executable"
test "`"$hb"`" = "Hello, world!" || fail "the td hello ($hb) did not greet when run directly"
echo "   ok: PATH hello = $hb (td's own build, executable, greets)"

# --- Leg C: DURABLE load-bearing (unknown package errors, NO guix fallback) ----
echo ">> [DURABLE load-bearing] an unknown package errors — no guix fallback"
if tdshell no-such-package-xyzzy -- true >/dev/null 2>"$cache/c.err"; then
  fail "td shell no-such-package-xyzzy SUCCEEDED — it must error, not fall back to guix"
fi
grep -q "no td recipe for" "$cache/c.err" \
  || { cat "$cache/c.err" >&2; fail "unknown-package failure was not the 'no td recipe' error (a guix fallback?)"; }
echo "   ok: errored with 'no td recipe for ...'; td shell does not reach for guix"

# --- Leg D: REMOVABLE guix oracle (distinct store path; same greeting) ---------
echo ">> [REMOVABLE oracle] td's hello is a DISTINCT store path from guix's"
gxdir=`guix build hello` || fail "guix build hello (oracle)"
test "$hb" != "$gxdir/bin/hello" || fail "td hello path equals guix's — not diverged"
td_base=`basename "$(dirname "$(dirname "$hb")")"`
gx_base=`basename "$gxdir"`
test "$td_base" != "$gx_base" || fail "td hello store basename equals guix's ($td_base) — not diverged"
test "`"$hb"`" = "`"$gxdir/bin/hello"`" || fail "td and guix hello greet differently"
echo "   ok: td=$td_base  vs guix=$gx_base (distinct paths, same greeting)"

echo "PASS: td shell builds td's OWN hello from its recipe (td-builder build-recipe — guix/Guile"
echo "      scrubbed from PATH, so no guix process) and runs the command with it on PATH; the"
echo "      binary is td's build at a td store path distinct from guix's; an unknown package"
echo "      errors with no guix fallback (North-Star step 1: td shell is guix-free)."
