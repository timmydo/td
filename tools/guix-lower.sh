#!/bin/sh
# guix-lower.sh — lower a td object to its derivation file name (default) or, with
# --out, build it and print its output path. The td-native replacement for the
# `tests/<x>-drv.scm` Guile "lowering bridge" scripts run under `guix repl`
# (move-off-Guile §5; the retire-lowering-bridges arc).
#
# Usage:
#   tools/guix-lower.sh        '<INNER>'   # prints the .drv file name (guix build -d)
#   tools/guix-lower.sh --out  '<INNER>'   # builds it, prints the output path
#
# INNER is a Scheme expression, evaluated with the repo on GUILE_LOAD_PATH, that —
# given the open store connection bound to `s` — returns a <derivation>. Examples:
#   monadic subject:  '((@@ (guix store) run-with-store) s ((@ (system td-registry) td-registry) #:gens (quote (1 2))))'
#   store-fn subject: '((@ (system td-build) td-rust-build-derivation) s (quote (...recipe...)))'
#
# WHY return a <derivation> directly (not a (lambda () monadic)): guix's
# `compute-derivation` (guix/scripts/build.scm) uses a <derivation> AS-IS, but wraps the
# procedure/gexp/file-like paths in `(set-guile-for-build (default-guile))`. The old
# bridges call `run-with-store` WITHOUT that, so to reproduce their `.drv`
# BYTE-IDENTICALLY (same output path, DIGESTS.md unchanged) the expression must yield a
# <derivation> — which is exactly what `(run-with-store s …)` returns here.
#
# `run-with-store` is private to (guix store) → `(@@ (guix store) …)`. We add the repo to
# GUILE_LOAD_PATH rather than passing `-L .` because `-L .` makes guix scan `.` as a
# package path (it then tries to compile ci/*.scm + tests/*.scm and emits a garbage drv
# list); GUILE_LOAD_PATH only makes the `(system …)`/`(tests …)` modules loadable.
#
# TD_GUIX overrides the guix invocation (default: the channels.scm time machine), so the
# loop sandbox passes its pinned `guix time-machine -C channels.scm --`.
set -eu

mode=-d
case "${1:-}" in
  --out) mode='' ; shift ;;
  -d)    mode=-d ; shift ;;
esac
[ "$#" -ge 1 ] || { echo "guix-lower.sh: missing INNER expression" >&2; exit 2; }
inner=$1

# Repo root = the parent of this script's tools/ dir; make its modules loadable.
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
GUILE_LOAD_PATH="$root${GUILE_LOAD_PATH:+:$GUILE_LOAD_PATH}"
export GUILE_LOAD_PATH

expr="(let* ((s ((@ (guix store) open-connection)))) (let ((d $inner)) ((@ (guix store) close-connection) s) d))"

# shellcheck disable=SC2086  # $mode is an intentional optional flag; TD_GUIX is a word list.
exec ${TD_GUIX:-guix time-machine -C channels.scm --} build $mode -e "$expr"
