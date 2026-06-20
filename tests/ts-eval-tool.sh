# tests/ts-eval-tool.sh BASEDIR — build td-ts-eval (td's boa-based JS evaluator) with the
# td-bootstrapped stage0 and print the td-built binary path (move-off-Guile §5 brick 4b).
# Shared by the `build-recipes` prelude (which then exports it as TD_TS_EVAL for the gnu
# build path) AND the rust-ts-eval gate — built ONCE into BASEDIR. The build goes through
# `td-builder build-recipe`'s content-addressed cache, so a WARM tree CACHE-HITS (a few
# seconds: intern + ts-emit + cached realize) and only a COLD/changed ts-eval/ rebuilds
# (~the corpus's per-package cost, once) — there is NO per-loop 15-min prelude.
#
# The td-built td-ts-eval produces byte-identical JSON to the guix-built one (same source;
# the rust-ts-eval gate's migration-oracle proves it), so the gnu gates can evaluate their
# recipes with it. The bootstrap circularity is honest: the guix-built SEED ($TD_TS_EVAL
# in) evaluates td-ts-eval's OWN recipe here; only this prelude resolves the seed.
#
# Env in (resolved by the caller, which has guix): TD_NODE, TD_TSC, TD_TSDIR (the ts-emit
# transpile), TD_TS_EVAL (the guix-built SEED), TB + TD_BUILDER_PATH/STORE/DB (stage0, from
# cache-lib load_stage0). The td-ts-eval.lock seed+crates must already be realized by the
# caller (`guix build`). Prints the td-built binary path; writes BASEDIR/tseval-path.
set -eu

base="${1:?usage: ts-eval-tool.sh BASEDIR}"
: "${TD_NODE:?}"; : "${TD_TSC:?}"; : "${TD_TSDIR:?}"; : "${TD_TS_EVAL:?}"
: "${TB:?}"; : "${TD_BUILDER_PATH:?}"; : "${TD_BUILDER_STORE:?}"; : "${TD_BUILDER_DB:?}"

lock0="tests/td-ts-eval.lock"
test -s "$lock0" || { echo "ts-eval-tool: no lock $lock0" >&2; exit 1; }
cu=$(grep -- '-coreutils-' "$lock0" | sed 's/^[^ ]* //' | head -1)
test -n "$cu" || { echo "ts-eval-tool: no coreutils in $lock0 for the scrubbed PATH" >&2; exit 1; }

mkdir -p "$base/tmp" "$base/b"; rm -f "$base/b/"*.drv

# Intern the ts-eval/ source with td's OWN recursive addToStore (no guix repl).
srcinfo=$(sh tests/intern-src.sh "$TB" td-ts-eval-src "ts-eval" "$base" target vendor .cargo) \
  || { echo "ts-eval-tool: could not intern the ts-eval source tree" >&2; exit 1; }
eval "$srcinfo"
test -n "${src:-}" -a -d "$srcstore/$(basename "$src")" \
  || { echo "ts-eval-tool: interned no ts-eval source tree" >&2; exit 1; }
lock="$base/td-ts-eval.lock"; { cat "$lock0"; echo "td-ts-eval-source $src"; } > "$lock"

# The guix-built SEED evaluates td-ts-eval's OWN recipe (the bootstrap circularity).
sh tests/ts-emit.sh "tests/ts/recipe-td-ts-eval.ts" > "$base/td-ts-eval.json" \
  || { echo "ts-eval-tool: the seed td-ts-eval could not evaluate recipe-td-ts-eval.ts" >&2; exit 1; }
test -s "$base/td-ts-eval.json" || { echo "ts-eval-tool: seed produced no JSON" >&2; exit 1; }

# Build td-ts-eval with stage0 (content-addressed cache: warm = CACHE=hit, instant).
env -i HOME="$base" TMPDIR="$base/tmp" PATH="$cu/bin" \
  TD_BUILDER_PATH="$TD_BUILDER_PATH" TD_BUILDER_STORE="$TD_BUILDER_STORE" TD_BUILDER_DB="$TD_BUILDER_DB" \
  "$TB" build-recipe "$base/td-ts-eval.json" "$lock" "$base/b" /var/guix/db/db.sqlite "$srcstore" "$srcdb" \
  > "$base/bout" 2>"$base/err" || { echo "ts-eval-tool: build-recipe failed:" >&2; tail -20 "$base/err" >&2; exit 1; }
out=$(sed -n 's/^OUT=out //p' "$base/bout")
test -n "$out" || { echo "ts-eval-tool: build-recipe produced no output" >&2; cat "$base/err" >&2; exit 1; }
bin="$base/b/newstore/$(basename "$out")/bin/td-ts-eval"
test -x "$bin" || { echo "ts-eval-tool: no td-ts-eval binary at $bin" >&2; exit 1; }

printf '%s\n' "$bin" > "$base/tseval-path"
echo "$bin"
