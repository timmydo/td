# tests/stage0-builder.sh BASEDIR — produce a STAGE0 td-builder (guix-free, via
# tools/bootstrap-td-builder.sh: cargo build under env -i with only the pinned toolchain
# on PATH) and PLACE it into a td-owned store under BASEDIR using STAGE0'S OWN
# `store-add-builder` — so NO guix-built td-builder is involved anywhere (stage0 places
# itself). Writes BASEDIR/{store/<base>/…, builder.db, .stage0-meta} and prints the
# placed builder's canonical store path (Cb).
#
# move-off-Guile §5, bootstrap brick 3: the package build path (build-recipes phase +
# the corpus/toolchain/corpus-deps/rust gates, via cache-lib) builds with THIS stage0,
# not `guix build -e '(@ (system td-builder) td-builder)'`. The toolchain SEED is the
# guix-built pin (tests/td-builder-rust.lock; retired LAST §5) — its paths are read as
# strings, not resolved by guix; the caller realizes the seed up front.
#
# Memoized: .stage0-meta records (builder-source fingerprint, Cb). A second call whose
# fingerprint matches AND whose placement is present reuses it (no rebuild) — so warm
# loops skip the ~8s compile; a CHANGED builder/ (new fingerprint) rebuilds + replaces.
set -eu

base="${1:?usage: stage0-builder.sh BASEDIR}"
lock="${TD_LOCK:-tests/td-builder-rust.lock}"
store="$base/store"
db="$base/builder.db"
meta="$base/.stage0-meta"
test -s "$lock" || { echo "stage0-builder: no toolchain lock $lock" >&2; exit 1; }

# Fingerprint the builder source the stage0 is compiled from — reuse only if unchanged.
fp=`find builder/src builder/Cargo.toml builder/Cargo.lock -type f -exec sha256sum {} + \
     | sort | sha256sum | cut -d' ' -f1`
if [ -f "$meta" ]; then
  oldfp=`sed -n 1p "$meta"`
  cb=`sed -n 2p "$meta"`
  if [ "$oldfp" = "$fp" ] && [ -n "$cb" ] && [ -x "$store/`basename "$cb"`/bin/td-builder" ] && [ -s "$db" ]; then
    echo "$cb"; exit 0
  fi
fi

work=`mktemp -d`
trap 'rm -rf "$work"' EXIT INT TERM
# 1. cargo-compile stage0 from builder/ source (guix/Guile-free, offline — the gate 170
#    bootstrap). Prints the binary path; cargo noise goes to stderr.
s0=`TD_LOCK="$lock" sh tools/bootstrap-td-builder.sh "$work/s0"`
test -x "$s0" || { echo "stage0-builder: bootstrap produced no stage0 td-builder" >&2; exit 1; }
# 2. stage0 places ITSELF into the td store (its OWN store-add-builder; refs scanned vs
#    the daemon db). No guix-built td-builder anywhere in the loop.
mkdir -p "$store"
cb=`"$s0" store-add-builder td-builder-0.1.0 "$work/s0" "$store" "$db" /var/guix/db/db.sqlite`
case "$cb" in
  /gnu/store/*-td-builder-0.1.0) : ;;
  *) echo "stage0-builder: store-add-builder gave a malformed path '$cb'" >&2; exit 1 ;;
esac
test -x "$store/`basename "$cb"`/bin/td-builder" || { echo "stage0-builder: stage0 not restored under $store" >&2; exit 1; }
printf '%s\n%s\n' "$fp" "$cb" > "$meta"
echo "$cb"
