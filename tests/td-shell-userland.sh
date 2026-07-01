#!/bin/sh
# tests/td-shell-userland.sh — the REAL `td shell` product command over the REAL shipped
# Rust userland, GUIX-FREE. This is the end-to-end USE-CASE gate: a person types
# `td shell ripgrep -- rg PATTERN tree` (and a multi-tool `td shell ripgrep fd -- …`) and
# the shipped tools build on demand and actually do their job — from td's OWN store paths,
# with no `guix` process anywhere in the resolve/build/exec path.
#
# It complements the per-tool `rust-<x>` gates (347 etc.): THOSE build each tool through the
# bespoke `crate-free-build.sh` harness and assert build==pin in isolation (supply-chain /
# structural / repro). This gate drives the actual PRODUCT command (`td-builder shell`), which
# until now could only build trivial seed packages (hello). The crate-closure provisioning a
# rust recipe needs (intern source + crate set → build-recipe's TD_VENDOR_DIR form) now lives
# in `td shell` itself, so the user-facing command builds the real userland. No bespoke
# harness in the build path; the assertions are all DURABLE behavioral (no guix oracle):
#
#   A [DURABLE behavioral]   `td shell ripgrep -- rg needle tree` finds the needle line (and
#                            NOT the unrelated file), with guix/Guile SCRUBBED from PATH.
#   B [DURABLE td-built]     the `rg` on the composed PATH is td's OWN build at a td store path
#                            (under the td-shell cache, distinct from any guix path), runnable.
#   C [DURABLE load-bearing] an unknown package errors ("no td recipe"), it does NOT fall back
#                            to guix.
#   D [DURABLE multi-tool]   `td shell ripgrep fd -- …` composes a real user environment: fd
#                            finds a file by name under the tree and runs rg inside the match —
#                            two td-built guix-free tools cooperating in one shell.
#
# Crate closures (ripgrep + fd) are warmed GUIX-FREE by the check.sh prelude (`td-feed warm
# crate`, the cargo-proxy verifying each .crate sha256 == the crates.io index cksum — the
# upstream pin, NOT a guix artifact) into .td-build-cache/crate-vendor/<pkg>/; the rust/gcc
# toolchain SEED stays guix-built (retired last by the source bootstrap).
set -eu

fail() { echo "FAIL: $*" >&2; exit 1; }

. tests/cache-lib.sh
export TD_STAGE0_BASE="`pwd`/.td-build-cache/stage0"
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
load_recipe_eval || fail "no td-built td-recipe-eval (the build-recipes prelude must run first)"
test -x "$TD_RECIPE_EVAL" || fail "td recipe evaluator not executable"
echo ">> td tools (guix-free): stage0=$TB  recipe-eval=$TD_RECIPE_EVAL"

# The warmed crate closures (host PREP) the rust recipes build from.
VENDOR_ROOT="`pwd`/.td-build-cache/crate-vendor"
for p in ripgrep fd; do
  test -d "$VENDOR_ROOT/$p/vendor" \
    || fail "$p crate closure not warmed at $VENDOR_ROOT/$p — HOST PREP \`td-feed warm crate' (check.sh prelude) must provision it first (the offline gate cannot egress)"
done

# A scrubbed PATH for the td shell process: coreutils + bash from ripgrep's pinned seed, NO
# guix/Guile — so a green run PROVES td shell used no guix process.
cu=`grep -- '-coreutils-' tests/ripgrep.lock | sed 's/^[^ ]* //' | head -1`
sh_=`grep -- '-bash-' tests/ripgrep.lock | sed 's/^[^ ]* //' | head -1`
test -n "$cu" -a -n "$sh_" || fail "no coreutils/bash in tests/ripgrep.lock"
if ls "$cu/bin" "$sh_/bin" | grep -qE '^(guix|guile)$'; then fail "guix/guile on the scrubbed PATH"; fi
SCRUB="$cu/bin:$sh_/bin"

# Warm each tool's pinned toolchain SEED closure offline (test setup; bare `guix build` of the
# lock's /gnu/store seed paths — NOT a packager form, NOT in td shell's path, the same warming
# every rust gate does).
for lk in tests/ripgrep.lock tests/fd.lock; do
  grep ' /gnu/store/' "$lk" | sed 's/^[^ ]* //' | sort -u | xargs guix build >/dev/null \
    || fail "could not realize the toolchain seed from $lk"
done

cache="`pwd`/.td-build-cache/td-shell-userland-pkgs"; rm -rf "$cache"; mkdir -p "$cache/tmp"

# td shell, run with guix/Guile OFF PATH (env -i + scrubbed PATH ⇒ no guix process). The crate
# closure is provisioned from TD_SHELL_VENDOR_ROOT. The store DB used to stage the toolchain
# seed closure is left to run_shell's own default (TD_SHELL_STORE_DB) — that seed-staging read
# is the existing product surface (retired by the unpacked seed store), so this gate does not
# re-spell it and grow the guix-db census (directive 8).
tdshell() {
  env -i HOME="$cache" TMPDIR="$cache/tmp" PATH="$SCRUB" \
    TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
    TD_RECIPE_EVAL="$TD_RECIPE_EVAL" \
    TD_SHELL_LOCKS=tests \
    TD_SHELL_CACHE="$cache" TD_SHELL_VENDOR_ROOT="$VENDOR_ROOT" \
    "$TB" shell "$@"
}

# A fixture tree: a needle hidden in one file, and an unrelated file that must NOT match.
tree="$cache/tree"; mkdir -p "$tree/sub"
printf 'alpha line\nthe needle is here\nbeta line\n' > "$tree/sub/hay.txt"
printf 'nothing to see\n' > "$tree/other.log"

# --- Leg A: DURABLE behavioral (build + run td's ripgrep over a real task, no guix) ---------
echo ">> [DURABLE behavioral] td shell ripgrep -- rg needle tree (guix/Guile OFF PATH)"
out=`tdshell ripgrep -- rg needle "$tree" 2>"$cache/a.err"` \
  || { tail -40 "$cache/a.err" >&2; fail "td shell ripgrep -- rg exited nonzero"; }
echo "$out" | grep -q 'needle' || fail "td-built rg did not find the 'needle' line (got: $out)"
echo "$out" | grep -q 'other.log' && fail "td-built rg matched the unrelated file (over-match)"
echo "   ok: td built its own ripgrep (no guix on PATH) and rg found the needle (not the unrelated file)"

# --- Leg B: DURABLE td-built (the rg on PATH is td's OWN build at a td store path) ----------
echo ">> [DURABLE td-built] the rg on PATH is td's own build at a td store path"
rb=`tdshell ripgrep -- bash -c 'command -v rg'` || fail "could not locate rg on the composed PATH"
case "$rb" in
  "$cache"/ripgrep/newstore/*-ripgrep-*/bin/rg) : ;;
  *) fail "rg on PATH is '$rb' — not a td-built path under $cache" ;;
esac
test -x "$rb" || fail "the td rg ($rb) is not executable"
echo "   ok: PATH rg = $rb (td's own build, executable)"

# --- Leg C: DURABLE load-bearing (unknown package errors, NO guix fallback) -----------------
echo ">> [DURABLE load-bearing] an unknown package errors — no guix fallback"
if tdshell no-such-package-xyzzy -- true >/dev/null 2>"$cache/c.err"; then
  fail "td shell no-such-package-xyzzy SUCCEEDED — it must error, not fall back to guix"
fi
grep -q "no td recipe for" "$cache/c.err" \
  || { cat "$cache/c.err" >&2; fail "unknown-package failure was not the 'no td recipe' error (a guix fallback?)"; }
echo "   ok: errored with 'no td recipe for ...'; td shell does not reach for guix"

# --- Leg D: DURABLE multi-tool (a real user environment: fd finds files, rg greps them) -----
echo ">> [DURABLE multi-tool] td shell ripgrep fd -- fd finds the file, rg greps inside it"
work=`tdshell ripgrep fd -- fd hay "$tree" -x rg needle 2>"$cache/d.err"` \
  || { tail -40 "$cache/d.err" >&2; fail "td shell ripgrep fd -- (fd -x rg) exited nonzero"; }
echo "$work" | grep -q 'needle' \
  || fail "the fd+rg pipeline did not surface the needle (got: $work) — multi-tool env broken"
# Both tools must be td-built store paths on the SAME composed PATH.
fb=`tdshell ripgrep fd -- bash -c 'command -v fd'` || fail "could not locate fd on the composed PATH"
case "$fb" in
  "$cache"/fd/newstore/*-fd-*/bin/fd) : ;;
  *) fail "fd on PATH is '$fb' — not a td-built path under $cache" ;;
esac
echo "   ok: fd ($fb) and rg both td-built, guix-free, cooperating in one td shell — a real userland"

echo "PASS: the REAL \`td shell' product command builds and runs the shipped Rust userland"
echo "      (ripgrep + fd) from its guix-free crate closure — guix/Guile scrubbed from PATH, so"
echo "      no guix process; rg greps a needle, fd+rg compose a real task, both at td store paths;"
echo "      an unknown package errors with no guix fallback. End-to-end use-case, all DURABLE."
