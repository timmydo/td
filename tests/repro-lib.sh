# repro-lib.sh — shared reproducibility normalization for the MODERN /td/store toolchain
# rungs (binutils 2.44 / glibc 2.41 / gcc 14.3.0). Sourced by the bootstrap-*-store-native
# gates; defines no gate of its own.
#
# WHY this exists. td's store path is content-addressed by the NAR hash, and td's NAR
# (builder/src/nar.rs) hashes ONLY: node type, the executable bit, file CONTENTS, symlink
# targets, and the sorted directory structure — NOT mtimes / uid / gid / non-exec mode bits.
# So filesystem "install mtimes" never move the path; only file *contents* (+ exec bit +
# structure) do. Two from-scratch builds of a modern rung built into a `mktemp -d` build dir
# still differ in three CONTENT ways, so the interned /td/store path varies build-to-build
# (measured on binutils 2.44: 35 differing files — 26 ELF, 6 archives, 3 libtool .la):
#
#   1. Build-path leak in DWARF. The rungs compile with the autoconf default `-g -O2`, so the
#      random absolute build path is baked into `.debug_*` (DW_AT_comp_dir / DW_AT_name) of
#      every ELF (cc1, ld, as, libc.so.6, …) — different every build.
#   2. Archive member mtimes. Installed `.a` files (libbfd.a, libgcc.a, libc.a, …) record each
#      member's build-time mtime/uid/gid, written by the build-time `ar` (the mesboot ar — NOT
#      deterministic; the `--enable-deterministic-archives` configure flag only changes the
#      default of the ar being BUILT, not the ar used during the build).
#   3. Build-path leak in libtool `.la` files. Their `relink_command` (and dependency paths)
#      record the absolute build dir.
#
# THE FIX (`repro_normalize_tree`, applied to the install tree before interning):
#   - `strip --strip-debug --enable-deterministic-archives` over every ELF and ar archive:
#     `--strip-debug` drops the `.debug_*` sections that carry the build path while KEEPING the
#     symbol table (so static libs/objects — libc.a, crt*.o, libgcc.a — still link); `-D`
#     rewrites every archive's member headers with zero mtime/uid/gid + a canonical mode.
#   - delete `*.la`: libtool link metadata the final toolchain does not use, and which leaks
#     the build path (standard reproducible-toolchain practice).
# strip is deterministic for a given input+flags, and two independently-built strips have
# identical CODE (they differ only in their own debug sections, which don't affect behavior),
# so normalize(buildA) and normalize(buildB) are byte-identical when the build is otherwise
# reproducible — which the gate proves with a double-build CA-path equality leg.
#
# This is a DURABLE transform (it removes content non-determinism with no guix oracle in the
# room); the gates' double-build equality leg is the durable intrinsic-reproducibility check.

# repro_normalize_tree DIR STRIP [LOADER LIBPATH]
#   DIR    — the installed tree to canonicalize in place.
#   STRIP  — a binutils `strip` (>= 2.24 for -D); the freshly-built modern binutils strip.
#   LOADER, LIBPATH — if STRIP is a /td/store-dynamic ELF (interp absent in the build
#            sandbox), the ld-linux.so.2 + its lib dir, so STRIP is run as
#            `LOADER --library-path LIBPATH STRIP …`. LIBPATH must be OUTSIDE DIR. Omit both
#            when STRIP runs natively.
# Returns non-zero (and prints the offending file) if strip fails on any binary.
repro_normalize_tree() {
  _rn_dir=$1; _rn_strip=$2; _rn_loader=${3:-}; _rn_libpath=${4:-}
  test -d "$_rn_dir" || { echo "repro_normalize_tree: not a directory: $_rn_dir" >&2; return 1; }
  test -x "$_rn_strip" || { echo "repro_normalize_tree: strip not executable: $_rn_strip" >&2; return 1; }
  # strip rewrites in place — make the tree writable; the write bit is NAR-invisible (NAR
  # hashes only the &0o100 execute bit), so this does not perturb the hash.
  chmod -R u+w "$_rn_dir" 2>/dev/null || true
  # Run strip from a COPY outside DIR: the tree contains its own bin/strip (and an arch
  # hardlink of it), and stripping the binary that is itself running SIGBUSes the process.
  _rn_tmp=`mktemp -d`
  cp "$_rn_strip" "$_rn_tmp/strip" || { echo "repro_normalize_tree: could not copy strip" >&2; rm -rf "$_rn_tmp"; return 1; }
  chmod u+rwx "$_rn_tmp/strip"
  _rn_strip="$_rn_tmp/strip"
  # (3) drop libtool .la archives (link metadata; leak the build path via relink_command).
  find "$_rn_dir" -type f -name '*.la' -exec rm -f {} +
  # (1)+(2) strip debug + deterministic archives over every ELF / ar archive.
  if find "$_rn_dir" -type f -print | while IFS= read -r _rn_f; do
       # Sniff the first 4 bytes: ELF (7f 45 4c 46) or ar archive ("!<ar" = 21 3c 61 72).
       _rn_m=$(od -An -tx1 -N4 "$_rn_f" 2>/dev/null | tr -d ' \n')
       case "$_rn_m" in
         7f454c46|213c6172)
           if [ -n "$_rn_loader" ]; then
             "$_rn_loader" --library-path "$_rn_libpath" "$_rn_strip" \
               --strip-debug --enable-deterministic-archives "$_rn_f" \
               || { echo "repro_normalize_tree: strip failed on $_rn_f" >&2; exit 17; }
           else
             "$_rn_strip" --strip-debug --enable-deterministic-archives "$_rn_f" \
               || { echo "repro_normalize_tree: strip failed on $_rn_f" >&2; exit 17; }
           fi ;;
         *) : ;;  # scripts, headers, ld scripts, text — content already deterministic
       esac
     done
  then _rn_rc=0; else _rn_rc=$?; fi
  rm -rf "$_rn_tmp"
  return $_rn_rc
}
