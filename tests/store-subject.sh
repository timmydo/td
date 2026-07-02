# tests/store-subject.sh — the shared subject-swap helper for the store-backend gate
# cluster (275/290/295/300/305/310, R3 of the guix-retirement ladder → #261). It produces
# a TD-BUILT subject artifact + its closure for the store-DB gates to exercise, with NO
# guix in the recipe — no `guix build [-d] hello`, no `guix gc`, no /var/guix read.
#
# WHY a helper: every one of the six gates used to start with the same two guix calls —
# `out=`guix build hello`; drv=`guix build -d hello`` — and then `guix gc -R` to get the
# closure. R3 swaps all three for td's OWN path, once, here:
#   • SUBJECT: td BUILDS GNU hello via the corpus build-recipe path (cache-HIT — the
#     `build-recipes` prelude already built it into the shared .td-build-cache/pkg), so the
#     subject is a td-assembled .drv realised by td's own daemon, NOT `guix build`.
#   • .drv:    the deriver is the .drv td ASSEMBLED (assemble-recipe, guix/Guile off PATH),
#     read off the build log — NOT `guix build -d`.
#   • CLOSURE: computed by CONTENT-SCANNING (`td-builder store-closure-scan`), NOT `guix gc`.
#
# The td daemon builds hello's OUTPUT tree into a build scratch's `newstore/<base>` while its
# deps come from the seed /gnu/store — the bytes are SPLIT across two stores. So the helper:
#   1) discovers hello's runtime closure guix-free with a MULTI-STORE content scan spanning
#      the seed /gnu/store (deps) AND the newstore (the output), rooted at the canonical out;
#   2) STAGES a self-contained, td-OWNED store ($SUBJ_STORE) that holds EVERY closure member
#      at its basename (the output copied from newstore, the deps from /gnu/store) — one
#      uniform prefix, so the gates' store ops (register / query / verify / gc / add-output)
#      all read real bytes and every reference resolves (by 32-char hash) within the store.
# Staging into a td-owned store is exactly the boundary these gates already assert (td writes
# only its OWN scratch store; the host /gnu/store is never touched).
#
# Usage:  . tests/store-subject.sh ; td_store_subject "$scratch"
# On success sets (and exports):
#   SUBJ_STORE     — the self-contained td-owned store dir (every closure member at <base>)
#   SUBJ_ROOT      — hello's output path IN that store ($SUBJ_STORE/<hello-base>), the GC root
#   SUBJ_CLOSURE   — a file listing every member as $SUBJ_STORE/<base> (sorted, deduped)
#   SUBJ_N         — the number of closure members
#   SUBJ_DRV       — hello's .drv path (td-ASSEMBLED; the deriver string), no `guix build -d`
#   SUBJ_LOCALDRV  — the on-disk assembled .drv FILE (its bytes; for store-add-referenced)
#   SUBJ_TREE      — hello's original output tree ($ns, in the daemon newstore)
# Returns non-zero with a FAIL message on any error.

td_store_subject() {
  _scr="$1"
  test -n "${_scr:-}" || { echo "FAIL: td_store_subject needs a scratch dir" >&2; return 1; }
  . tests/cache-lib.sh
  export TD_STAGE0_BASE="${TD_STAGE0_BASE:-$(pwd)/.td-build-cache/stage0}"
  load_stage0 || return 1
  load_recipe_eval || return 1
  CU=`grep -- '-coreutils-' tests/hello-no-guix.lock | sed 's/^[^ ]* //' | head -1`
  test -n "$CU" || { echo "FAIL: no coreutils in tests/hello-no-guix.lock for the scrubbed PATH" >&2; return 1; }
  export CU
  case "$TD_RECIPE_EVAL" in *.td-build-cache/*) : ;; *) echo "FAIL: TD_RECIPE_EVAL is not td's own build ($TD_RECIPE_EVAL)" >&2; return 1 ;; esac

  # td BUILDS the subject. The assemble scratch is PER-GATE ($_scr/pkgcache), NOT the shared
  # .td-build-cache/pkg, so several store gates running concurrently (make -j2) never race on
  # one spec's assemble dir. The DAEMON build is still a HIT: hello's .drv is deterministic,
  # so the same canonical .drv the build-recipes prelude already realised is found in the
  # shared daemon store (no guix, no rebuild); `$ns` is the daemon's shared output tree.
  export CACHE="$_scr/pkgcache"; mkdir -p "$CACHE"
  cached_build hello tests/hello-no-guix.lock || return 1
  test -n "${out:-}" -a -n "${ns:-}" || { echo "FAIL: cached_build hello set no out/ns" >&2; return 1; }
  test -d "$ns" || { echo "FAIL: hello's output tree $ns is absent" >&2; return 1; }
  _hbase=`basename "$out"`

  # The deriver = the .drv td ASSEMBLED (assemble-recipe printed its canonical /gnu/store path
  # to the build log); NOT `guix build -d`. And the assembled .drv FILE (its bytes).
  SUBJ_DRV=`grep -hoE '/gnu/store/[a-z0-9]+-hello-[^ ]+\.drv' "$sd/err" "$sd/bout" 2>/dev/null | head -1`
  test -n "$SUBJ_DRV" || { echo "FAIL: could not read the td-ASSEMBLED hello .drv path from the build log" >&2; return 1; }
  SUBJ_LOCALDRV=`ls "$sd/b/"*.drv 2>/dev/null | head -1`
  test -n "${SUBJ_LOCALDRV:-}" && [ -f "$SUBJ_LOCALDRV" ] || { echo "FAIL: no assembled hello .drv file under $sd/b" >&2; return 1; }

  # 1) DISCOVER hello's runtime closure guix-free: a MULTI-STORE content scan spanning the
  #    seed /gnu/store (deps) + the daemon newstore (the output tree), rooted at the canonical
  #    out. `store-closure-scan` == the daemon's scanForReferences (gate 290) — no `guix gc`.
  _nsp=`dirname "$ns"`
  _closure=`"$TB" store-closure-scan "/gnu/store,$_nsp" "$out"` \
    || { echo "FAIL: store-closure-scan could not close hello ($out)" >&2; return 1; }
  test -n "$_closure" || { echo "FAIL: empty runtime closure for $out" >&2; return 1; }

  # 2) STAGE a self-contained td-owned store: every member at $SUBJ_STORE/<base>, its bytes
  #    copied from wherever the SAME multi-store scan found them — the seed /gnu/store OR the
  #    newstore (the scan canonicalises to /gnu/store/<base> but a member's bytes may live in
  #    either dir, so resolve each by probing both, /gnu/store first to match the scan's dir
  #    precedence). This keeps staging self-consistent with the split-store discovery instead
  #    of assuming every non-output member is seed-only.
  SUBJ_STORE="$_scr/allstore"; rm -rf "$SUBJ_STORE"; mkdir -p "$SUBJ_STORE"
  : > "$_scr/closure.txt"
  for _p in $_closure; do
    _b=`basename "$_p"`
    if [ -e "/gnu/store/$_b" ]; then _src="/gnu/store/$_b"
    elif [ -e "$_nsp/$_b" ]; then _src="$_nsp/$_b"
    else echo "FAIL: closure member $_b has no bytes in /gnu/store or $_nsp" >&2; return 1; fi
    cp -a "$_src" "$SUBJ_STORE/$_b" || { echo "FAIL: could not stage $_b into $SUBJ_STORE" >&2; return 1; }
    printf '%s\n' "$SUBJ_STORE/$_b" >> "$_scr/closure.txt"
  done
  chmod -R u+w "$SUBJ_STORE" || { echo "FAIL: could not make the staged store writable" >&2; return 1; }
  sort -u "$_scr/closure.txt" -o "$_scr/closure.txt" || { echo "FAIL: could not sort the staged closure" >&2; return 1; }

  SUBJ_CLOSURE="$_scr/closure.txt"
  SUBJ_ROOT="$SUBJ_STORE/$_hbase"
  SUBJ_TREE="$ns"
  SUBJ_N=`wc -l < "$SUBJ_CLOSURE"`
  export SUBJ_STORE SUBJ_ROOT SUBJ_CLOSURE SUBJ_DRV SUBJ_LOCALDRV SUBJ_TREE SUBJ_N
  test -d "$SUBJ_ROOT" || { echo "FAIL: staged subject root $SUBJ_ROOT is absent" >&2; return 1; }
  test "$SUBJ_N" -ge 1 || { echo "FAIL: staged closure is empty" >&2; return 1; }
  echo "   [td-subject] hello built by td (cache-hit, no guix); $SUBJ_N-path runtime closure content-scanned + staged into the td-owned store $SUBJ_STORE"
}
