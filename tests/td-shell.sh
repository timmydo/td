#!/bin/sh
# tests/td-shell.sh — behavioral gate for `td-builder shell`, td's own `guix shell`.
#
# `td shell PKG... -- CMD...` brings the named packages into CMD's environment and
# runs CMD. This is the "own, then diverge" split (CLAUDE.md): the package layer
# (name -> derivation -> output) stays on the guix ORACLE for v1 — `guix build PKG`,
# the same resolution guix shell does, the move-off-Guile §5 layer retired LAST —
# but the ENVIRONMENT COMPOSITION + exec is td's OWN (td prepends each resolved
# output's bin/sbin to PATH itself and runs CMD, with no guix process in the exec
# path). So the merit here is DURABLE: the command actually runs with the package
# on PATH, an assertion that still holds with no guix in the room.
#
# The td-builder under test is the STAGE0 binary (tests/stage0-builder.sh:
# cargo-compiled from the CURRENT builder/ source, guix-free, placed by stage0
# itself) — so this gate needs NO `guix build -e '(@ (system td-builder) ...)'`
# packager site (guix-surface stays put) and exercises the source in this branch.
#
# Legs:
#   A [DURABLE behavioral]  `td shell hello -- hello` prints exactly "Hello, world!"
#   B [DURABLE structural]  the hello on the composed PATH is a real /gnu/store
#                           binary that itself runs and greets — the package
#                           injected a runnable hello (no guix to make this true)
#   C [DURABLE discriminate] WITHOUT the package, `td shell -- hello` FAILS in the
#                           SAME env where WITH it succeeds — the package is
#                           load-bearing, not a pass-through; a bogus package name
#                           fails loudly too (resolution is real)
#   D [REMOVABLE oracle]    td resolves hello to guix's exact package output, and
#                           `td shell hello -- hello` == `guix shell hello -- hello`
#                           — the guix differential, DELETED (not rewritten) when
#                           guix retires; the durable legs A–C are what remain
set -eu

work=`mktemp -d`
trap 'rm -rf "$work"' EXIT INT TERM

fail() { echo "FAIL: $*" >&2; exit 1; }

# --- build the td-builder under test (stage0, guix-free, current source) -------
s0base="`pwd`/.td-build-cache/td-shell"
cb=`sh tests/stage0-builder.sh "$s0base"` \
  || fail "stage0-builder could not place a stage0 td-builder"
tb="$s0base/store/`basename "$cb"`/bin/td-builder"
test -x "$tb" || fail "stage0 td-builder not executable at $tb"
echo ">> td-builder under test (stage0, guix-free): $tb"

# --- Leg A: DURABLE behavioral -------------------------------------------------
echo ">> [DURABLE behavioral] td shell hello -- hello"
"$tb" shell hello -- hello > "$work/a.out" 2>"$work/a.err" \
  || { echo "--- stderr ---" >&2; cat "$work/a.err" >&2; fail "td shell hello -- hello exited nonzero"; }
test "`cat "$work/a.out"`" = "Hello, world!" \
  || fail "td shell hello -- hello printed `cat "$work/a.out"` (expected 'Hello, world!')"
echo "   ok: hello ran in the composed env and greeted"

# --- Leg B: DURABLE structural (the package injected a runnable hello) ---------
echo ">> [DURABLE structural] the hello on the composed PATH is a real store binary"
hb=`"$tb" shell hello -- sh -c 'command -v hello'` \
  || fail "could not locate hello on the composed PATH"
case "$hb" in
  /gnu/store/*/bin/hello) : ;;
  *) fail "hello resolved to '$hb', not a /gnu/store .../bin/hello" ;;
esac
test -x "$hb" || fail "the composed-PATH hello ($hb) is not executable"
test "`"$hb"`" = "Hello, world!" \
  || fail "the composed-PATH hello ($hb) did not greet when run directly"
echo "   ok: PATH-head hello = $hb (executable, greets)"

# --- Leg C: DURABLE self-discrimination (the package is load-bearing) ---------
echo ">> [DURABLE discriminate] without the package, the same command must FAIL"
command -v hello >/dev/null 2>&1 \
  && fail "precondition broken: 'hello' is already on this gate's PATH — the without-package leg can't discriminate"
if "$tb" shell -- hello >/dev/null 2>&1; then
  fail "td shell -- hello (no package) SUCCEEDED — the package is not load-bearing"
fi
echo "   ok: td shell -- hello (no package) fails; td shell hello -- hello (Leg A) succeeds"
echo ">> [DURABLE discriminate] a bogus package name fails loudly"
if "$tb" shell no-such-package-xyzzy -- true >/dev/null 2>"$work/c.err"; then
  fail "td shell no-such-package-xyzzy succeeded — resolution is a no-op"
fi
grep -q "no-such-package-xyzzy" "$work/c.err" \
  || fail "bogus-package failure did not name the package (resolution not reached?)"
echo "   ok: a bogus package is rejected at resolution"

# --- Leg D: REMOVABLE guix oracle (delete when guix retires) -------------------
echo ">> [REMOVABLE oracle] td resolves hello to guix's exact package output"
oracle=`guix build hello` || fail "guix build hello failed (oracle)"
test "$hb" = "$oracle/bin/hello" \
  || fail "td put $hb on PATH; the guix package output is $oracle/bin/hello"
echo "   ok: td's PATH hello == \$(guix build hello)/bin/hello"
echo ">> [REMOVABLE oracle] td shell == guix shell (same greeting)"
guix shell hello -- hello > "$work/d.guix" 2>/dev/null \
  || fail "guix shell hello -- hello failed (oracle)"
test "`cat "$work/d.guix"`" = "`cat "$work/a.out"`" \
  || fail "td shell output != guix shell output"
echo "   ok: td shell hello -- hello byte-equals guix shell hello -- hello"

echo "PASS: td shell brings a package into a command's env and runs it — behavioral"
echo "      (hello greets), structural (a real store hello on PATH), and"
echo "      load-bearing (no package -> fail); guix-shell-equivalent (removable oracle)."
