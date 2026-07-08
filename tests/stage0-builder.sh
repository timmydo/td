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
td_self="${TD_BUILDER_SELF:?stage0-builder requires TD_BUILDER_SELF for source fingerprinting}"
test -x "$td_self" || { echo "stage0-builder: TD_BUILDER_SELF is not executable: $td_self" >&2; exit 1; }

# Fingerprint the builder source the stage0 is compiled from — reuse only if unchanged.
fp=`"$td_self" tree-fingerprint builder/src builder/build.rs builder/Cargo.toml builder/Cargo.lock`
# A valid memo: the fingerprint matches AND the placement + db are present. Sets $cb.
memo_hit() {
  [ -f "$meta" ] || return 1
  {
    IFS= read -r oldfp || oldfp=
    IFS= read -r cb || cb=
  } < "$meta"
  [ "$oldfp" = "$fp" ] && [ -n "$cb" ] \
    && [ -x "$store/`basename "$cb"`/bin/td-builder" ] && [ -s "$db" ]
}
# Fast path: a valid memo needs no lock (warm loops skip the compile AND the flock).
if memo_hit; then echo "$cb"; exit 0; fi

# Slow path: serialize build+place across concurrent gates sharing this BASEDIR. The
# check-engine smoke tier runs several stage0-using gates with NO build-recipes to place
# stage0 first, so they all re-place the SAME shared stage0 at once; without this lock their
# concurrent `store-add-builder` collide ("File exists (os error 17)"). flock is from
# util-linux (exposed by check.sh); the lock releases when fd 9 closes on exit.
mkdir -p "$base"
exec 9>"$base/.stage0.lock"
flock 9
# Double-checked: a gate that waited for the lock may now find the holder's fresh memo —
# reuse it rather than rebuild+re-place into the same store.
if memo_hit; then echo "$cb"; exit 0; fi

work=`mktemp -d`
trap 'rm -rf "$work"' EXIT INT TERM
# 1. cargo-compile stage0 from builder/ source (guix/Guile-free, offline — the gate 170
#    bootstrap). Prints the binary path; cargo noise goes to stderr.
s0=`TD_LOCK="$lock" sh tools/bootstrap-td-builder.sh "$work/s0"`
test -x "$s0" || { echo "stage0-builder: bootstrap produced no stage0 td-builder" >&2; exit 1; }
# 2. stage0 places ITSELF into the td store (its OWN store-add-builder; refs scanned vs
#    the seed store dir's entries — a readdir, NO /var/guix/db read (#313), so a guix-less
#    host cold-starts: /gnu/store absent → no candidates → no refs, exactly right for a
#    rustup/system-cc stage0 that embeds no store paths). No guix-built td-builder
#    anywhere in the loop.
mkdir -p "$store"
cb=`"$s0" store-add-builder td-builder-0.1.0 "$work/s0" "$store" "$db" /gnu/store`
case "$cb" in
  /gnu/store/*-td-builder-0.1.0) : ;;
  *) echo "stage0-builder: store-add-builder gave a malformed path '$cb'" >&2; exit 1 ;;
esac
test -x "$store/`basename "$cb"`/bin/td-builder" || { echo "stage0-builder: stage0 not restored under $store" >&2; exit 1; }
printf '%s\n%s\n' "$fp" "$cb" > "$meta"

# 3. GC stale placements (#309). This slow path just placed the CURRENT stage0 ($cb)
#    and store-add-builder rewrote builder.db to reference ONLY it, so every OTHER
#    *-td-builder-* directory under $store is a placement from an earlier builder/
#    fingerprint (a new fingerprint ⇒ new content-addressed $cb ⇒ a fresh dir, and the
#    old one was never removed). Left alone they accumulate one-per-change on a
#    long-lived warm runner (unbounded disk) and are a latent hazard for any glob-style
#    resolver (the #293 daemon-budget red). Sweep them.
#
#    Concurrency-safe: we still hold the .stage0.lock (fd 9, open until this script
#    exits), so no other stage0-builder.sh can be placing here, and load_stage0 only
#    ever resolves the CURRENT $cb (from .stage0-meta / the memo) — which we KEEP —
#    within the same per-BASEDIR store; a fast-path resolver returns $cb only when its
#    fingerprint matches the meta, i.e. the very dir we preserve. Only the slow path
#    (which holds the lock) ever creates or removes placements.
cur=`basename "$cb"`
swept=0
for d in "$store"/*-td-builder-*; do
  [ -e "$d" ] || continue          # no glob match ⇒ the literal pattern; skip
  [ -d "$d" ] || continue
  b=`basename "$d"`
  if [ "$b" = "$cur" ]; then continue; fi   # keep the current placement
  # Best-effort: a failed rm (transient EBUSY, perms) must never fail the PLACEMENT —
  # the current stage0 is already placed and the memo written; the next slow path retries.
  rm -rf "$d" 2>/dev/null || continue
  swept=$((swept + 1))
done
if [ "$swept" -gt 0 ]; then
  echo "stage0-builder: swept $swept stale placement(s) from $store (kept $cur)" >&2
fi

echo "$cb"
