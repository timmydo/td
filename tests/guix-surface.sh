#!/bin/sh
# tests/guix-surface.sh — ratchet td's guix-as-PACKAGER surface toward zero
# (move-off-Guile §5 enforcement; sibling to tests/guix-dependence.scm).
#
# move-off-Guile (§5) removes guix from td's BUILD path: the build TOOL
# (td-builder->stage0), the EVALUATOR (td-recipe-eval), the transpiler (node->td-tsgo).
# The principle behind every step: an external seed is a pinned FIXED-OUTPUT FETCH
# the loop realises and td PLACES (store-add-recursive) — NOT a guix
# `(build-system ...)` package td asks the guix daemon to build via
# `guix build -e '(@ (system M) PKG)'`. That "guix-as-packager" surface is what
# this gate forbids from GROWING, so the §5 metric is enforced, not aspirational.
#
# It STATICALLY scans the loop's orchestration sources — the compiled gate
# bodies (builder/src/gate_defs/*.rs — their bash scripts invoke guix via the
# $TD_GUIX time-machine prefix the runner exports), tests/*.sh, ci/*.sh (minus
# itself) — for that invocation form, classifies the resolved
# `(@ (system M) NAME)` by reading system/M.scm (a `(package ...)` define =
# PACKAGER; an `(origin ...)`/fetch define = an allowed FETCHER seed), and records
# the sorted set of PACKAGER sites in tests/guix-surface.expected. A one-way
# RATCHET: FAIL if any current packager site is absent from the snapshot (the
# surface grew); PASS when the set only shrinks (a retiring track removed a seed —
# re-baseline freely to lock the win). Growing it needs a deliberate .expected
# edit, called out in the PR (CLAUDE.md directive 3) + sign-off.
#
# Static, offline, no guix invoked → cheap pool. Re-baseline:
#   TD_SURFACE_WRITE=1 ./check.sh guix-surface
# (or: TD_SURFACE_WRITE=1 sh tests/guix-surface.sh).
#
# Honest boundaries:
#  - Comment lines (leading #) are excluded, so the doc/examples here and the
#    "what we DON'T do" comments in cache-lib.sh / stage0-builder.sh don't count.
#  - The scanner excludes ITSELF: its match patterns contain the trigger token.
#  - specification->package lives in .scm (the resolver axis), out of this scope;
#    it is tracked by the guix-dependence census + the resolve gate, not here.
set -eu

self="tests/guix-surface.sh"
expected="tests/guix-surface.expected"
TAB=$(printf '\t')

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT INT TERM

# in-scope orchestration files (the scanner excludes itself). ci/*.sh — the CI
# store-image host-prep scripts — are in scope too, so directive 8's "the guix
# surface only shrinks" covers the CI staging path, not just the in-sandbox loop.
: > "$work/scope"
# (the Makefile + mk/gates/*.mk entries became builder/src/gate_defs/*.rs when the
# gate runner replaced make; check_loop.rs — the ported check.sh host prelude — stays
# out of scope exactly as the shell check.sh always was)
for f in builder/src/gate_defs/*.rs tests/*.sh ci/*.sh; do
  [ -f "$f" ] || continue
  [ "$f" = "$self" ] && continue
  printf '%s\n' "$f" >> "$work/scope"
done

# classify a `(system MOD) NAME` ref by its define in system/MOD.scm:
# prints "package" or "origin"; aborts (exit 2) if it cannot tell — we refuse to
# under-count the packager surface by silently mis-reading a seed.
classify() {
  cmod=$1; cname=$2
  cf="system/$cmod.scm"
  if [ ! -f "$cf" ]; then
    echo "guix-surface: (system $cmod) maps to missing $cf — cannot classify $cname" >&2
    exit 2
  fi
  cln=$(grep -nE "^\(define(-public)? ${cname}([ )]|$)" "$cf" | head -1 | cut -d: -f1)
  if [ -z "${cln:-}" ]; then
    echo "guix-surface: no (define $cname ...) in $cf — cannot classify" >&2
    exit 2
  fi
  # bound the search to THIS define's body — up to the next top-level (define …) —
  # so a short fetch define above a package define can't bleed into it (over-count).
  cend='$'
  for d in $(grep -nE "^\(define" "$cf" | cut -d: -f1); do
    if [ "$d" -gt "$cln" ]; then cend=$((d - 1)); break; fi
  done
  cctor=$(sed -n "${cln},${cend}p" "$cf" \
          | grep -oE "\((package|origin|url-fetch|git-fetch)" | head -1 | tr -d '(')
  case "$cctor" in
    package) echo package ;;
    origin | url-fetch | git-fetch) echo origin ;;
    *) echo "guix-surface: cannot classify $cname in $cf (constructor='${cctor:-?}')" >&2; exit 2 ;;
  esac
}

# --- gather active (non-comment) `guix build -e '(@ (system M) X)'` hits -------
: > "$work/hits"
while IFS= read -r f; do
  grep -nE "(guix|GUIX\)|TD_GUIX) build.*-e.*\(@ \(system " "$f" 2>/dev/null \
    | grep -vE "^[0-9]+:[[:space:]]*(#|//)" \
    | sed "s|^|$f:|" >> "$work/hits" || true
done < "$work/scope"

# --- classify each ref; collect the PACKAGER sites ----------------------------
: > "$work/cur.raw"
while IFS= read -r hit; do
  [ -n "$hit" ] || continue
  ref=$(printf '%s\n' "$hit" \
        | sed -nE "s/.*\(@ \(system ([a-z0-9-]+)\) ([a-zA-Z0-9_-]+)\).*/\1 \2/p" | head -1)
  [ -n "$ref" ] || continue
  f=${hit%%:*}
  mod=${ref% *}
  name=${ref#* }
  kind=$(classify "$mod" "$name") \
    || { echo "guix-surface: classification aborted for (system $mod) $name" >&2; exit 2; }
  if [ "$kind" = package ]; then
    printf '%s%s(system %s) %s\n' "$f" "$TAB" "$mod" "$name" >> "$work/cur.raw"
  fi
done < "$work/hits"
sort -u "$work/cur.raw" > "$work/cur"

cur_n=$(grep -c . "$work/cur" || true)

# --- SHRINK ratchet (CLAUDE.md directive 8): the guix surface may only shrink --------
# Beyond the packager axis, directive 8 forbids GROWING any "should-only-shrink" guix
# reliance. We ratchet a SITE SET — (file, category) pairs — over the categories that
# do not grow in normal package work and that a regression like a load-bearing guix
# read would add: `guix repl`/`guix system` (Guile lowering, retired last), `guix shell`,
# `guix gc`, and reads of guix's PRIVATE store DB (/var/guix/db). The per-package repro
# oracle (`guix build [--check]`) is NOT ratcheted — it grows with each package and
# retires wholesale with guix. Narration (a leading echo/printf) and comments don't
# count; only real command lines. A NEW site fails (directive 8); the set may shrink
# freely (re-baseline to lock the win). This is what would have caught the load-bearing
# `guix repl` + /var/guix/db read an earlier store-of-record gate tried to add.
#
# KNOWN COARSENESS (each site is a per-file boolean, greps are substring regexes):
#  - the `gc` regex matches INSIDE `guix gcc-toolchain`, so a real code line merely
#    mentioning the guix gcc-toolchain (e.g. a fail-message string) records a PHANTOM
#    `gc` site — confirm a `gc` site with a real `guix gc` command line before
#    treating it as one. Phantom sites still only-shrink honestly, so the ratchet
#    stays sound; the census counts just read high.
#  - CONCURRENT SHRINK PRs: two branches removing DIFFERENT sites 3-way-merge
#    textually clean but leave a STALE `surface-sites:` header count. Never
#    hand-merge the .expected files — after rebasing, REGENERATE both
#    (TD_SURFACE_WRITE=1 sh tests/guix-surface.sh) and re-verify with a no-write run
#    (the compare greps the header out, but the count should be right for humans).
shrink_expected="tests/guix-surface-shrink.expected"
: > "$work/shrink.cur"
while IFS= read -r f; do
  [ -f "$f" ] || continue
  # real command lines only: drop comments and leading-echo/printf narration
  # (|| true: a file with no surviving lines must not trip `set -e`)
  body=$(grep -vE "^[[:space:]]*(#|//)" "$f" | grep -vE "^[[:space:]]*(@?echo|printf)([[:space:]]|\")" || true)
  printf '%s\n' "$body" | grep -qE "(guix|GUIX\)|TD_GUIX) (repl|system)" && printf '%s\t%s\n' "$f" "lowering" >> "$work/shrink.cur"
  printf '%s\n' "$body" | grep -qE "(guix|GUIX\)|TD_GUIX) shell"          && printf '%s\t%s\n' "$f" "shell"    >> "$work/shrink.cur"
  printf '%s\n' "$body" | grep -qE "(guix|GUIX\)|TD_GUIX) gc"             && printf '%s\t%s\n' "$f" "gc"       >> "$work/shrink.cur"
  printf '%s\n' "$body" | grep -qE "/var/guix/db"                 && printf '%s\t%s\n' "$f" "guix-db-read" >> "$work/shrink.cur"
done < "$work/scope"
sort -u "$work/shrink.cur" -o "$work/shrink.cur"
shrink_n=$(grep -c . "$work/shrink.cur" || true)

# --- compact informational census (trend; NOT ratcheted) ----------------------
: > "$work/code"
while IFS= read -r f; do
  grep -vE "^[[:space:]]*(#|//)" "$f" >> "$work/code" || true
done < "$work/scope"
occ() { grep -oE "$1" "$work/code" 2>/dev/null | grep -c . || true; }
census() {
  # realize: plain `guix build <paths>` lines (seed realizes + build oracles) — the set
  # #311 retires site-by-site via td-subst (tools/resolve-seed.sh). Counted per LINE,
  # minus the --check oracle and the packager `-e (@ (system …))` form counted above.
  # Coarse like the rest of the census (a fail-message quoting `guix build` counts);
  # informational only, so the trend is what matters.
  sr_n=$(grep -E "(guix|GUIX)[\"')}]* build" "$work/code" 2>/dev/null \
         | grep -v -- '--check' | grep -v '(@ (system' | grep -c . || true)
  echo ">> guix-surface census (informational; only the packager set is ratcheted):"
  echo "   packager  guix build -e (system M) <package> : $cur_n   <-- RATCHETED (move-off-Guile §5)"
  echo "   oracle    guix build --check                 : $(occ '(guix|GUIX\)|TD_GUIX) build --check')   (kept: the repro oracle, retired with guix)"
  echo "   lowerer   guix repl / guix system            : $(occ '(guix|GUIX\)|TD_GUIX) (repl|system)')   (Guile config/lowering, retired last)"
  echo "   gc        guix gc                            : $(occ '(guix|GUIX\)|TD_GUIX) gc')"
  echo "   realize   guix build <pinned store paths>    : $sr_n   (seed realizes, retiring via td-subst — re #311)"
}

# --- WRITE / COMPARE ----------------------------------------------------------
if [ -n "${TD_SURFACE_WRITE:-}" ]; then
  {
    echo "# tests/guix-surface.expected — guix-as-PACKAGER ratchet snapshot (move-off-Guile §5)"
    echo "# Generated by tests/guix-surface.sh. A \"packager site\" = a loop-orchestration"
    echo "# file that resolves a guix (package ...) seed via 'guix build -e (@ (system M) NAME)'."
    echo "# The gate FAILS if any site is NOT listed here (the surface may only SHRINK);"
    echo "# growing it needs a deliberate edit here, called out in the PR (directive 3)."
    echo "# Re-baseline: TD_SURFACE_WRITE=1 ./check.sh guix-surface"
    echo "# Each line: <file><TAB>(system <mod>) <name>"
    echo "packager-sites: $cur_n"
    cat "$work/cur"
  } > "$expected"
  {
    echo "# tests/guix-surface-shrink.expected — guix-surface SHRINK ratchet (CLAUDE.md directive 8)"
    echo "# Generated by tests/guix-surface.sh. Each line is a (file, category) SITE for a"
    echo "# should-only-shrink guix reliance: lowering (guix repl/system), shell (guix shell),"
    echo "# gc (guix gc), guix-db-read (/var/guix/db). The per-package repro oracle"
    echo "# (guix build [--check]) is NOT here. The gate FAILS on any NEW site (the surface"
    echo "# may only SHRINK); growth needs a deliberate edit here, called out in the PR for"
    echo "# sign-off (directive 8). Re-baseline: TD_SURFACE_WRITE=1 ./check.sh guix-surface"
    echo "# Each line: <file><TAB><category>"
    echo "surface-sites: $shrink_n"
    cat "$work/shrink.cur"
  } > "$shrink_expected"
  census
  echo ">> WROTE baseline $expected ($cur_n packager sites) + $shrink_expected ($shrink_n shrink sites)"
  exit 0
fi

if [ ! -f "$expected" ]; then
  echo "FAIL: $expected missing — baseline first: TD_SURFACE_WRITE=1 ./check.sh guix-surface" >&2
  exit 1
fi

# baseline site set (drop the header comments + the count line)
grep -vE "^#|^packager-sites:" "$expected" | sed '/^$/d' | sort -u > "$work/base"

new=$(comm -13 "$work/base" "$work/cur" || true)
removed=$(comm -23 "$work/base" "$work/cur" || true)

census

if [ -n "$new" ]; then
  {
    echo ""
    echo "FAIL: guix-as-packager surface GREW — new site(s) resolving a guix (package ...)"
    echo "seed via 'guix build -e (@ (system M) NAME)' that are not in $expected:"
    printf '%s\n' "$new" | sed 's/^/  + /'
    echo ""
    echo "move-off-Guile §5: provision a new seed td-native — a pinned fixed-output fetch"
    echo "the loop realises + td's own placement (store-add-recursive) — NOT a guix"
    echo "(build-system ...) package. If this growth is genuinely intended, add the line(s)"
    echo "to $expected and call it out in the PR for sign-off (CLAUDE.md directive 3)."
  } >&2
  exit 1
fi

if [ -n "$removed" ]; then
  rn=$(printf '%s\n' "$removed" | grep -c .)
  echo ">> ratchet slack: $rn packager site(s) retired since baseline — re-baseline"
  echo "   (TD_SURFACE_WRITE=1 ./check.sh guix-surface) to lock the win into $expected."
fi

# --- SHRINK ratchet compare (directive 8) -------------------------------------
if [ ! -f "$shrink_expected" ]; then
  echo "FAIL: $shrink_expected missing — baseline first: TD_SURFACE_WRITE=1 ./check.sh guix-surface" >&2
  exit 1
fi
grep -vE "^#|^surface-sites:" "$shrink_expected" | sed '/^$/d' | sort -u > "$work/shrink.base"
snew=$(comm -13 "$work/shrink.base" "$work/shrink.cur" || true)
sremoved=$(comm -23 "$work/shrink.base" "$work/shrink.cur" || true)
if [ -n "$snew" ]; then
  {
    echo ""
    echo "FAIL: the guix surface GREW (CLAUDE.md directive 8 — it may only shrink) — new"
    echo "should-only-shrink guix site(s) not in $shrink_expected:"
    printf '%s\n' "$snew" | sed 's/^/  + /'
    echo ""
    echo "directive 8: a new PR must not add a guix dependency (a guix process, a read of"
    echo "guix's private state /var/guix/db, or a guix-built byte). The ONE sanctioned use"
    echo "is a LABELED removable migration oracle (directive 4), never load-bearing. If this"
    echo "growth is a genuine removable oracle, re-baseline (TD_SURFACE_WRITE=1 ./check.sh"
    echo "guix-surface) and call it out in the PR for sign-off (directive 3)."
  } >&2
  exit 1
fi
if [ -n "$sremoved" ]; then
  srn=$(printf '%s\n' "$sremoved" | grep -c .)
  echo ">> shrink slack: $srn guix site(s) retired since baseline — re-baseline to lock the win."
fi
echo ">> PASS: no new guix-as-packager surface, and the should-shrink guix surface did not grow (directive 8)."
exit 0
