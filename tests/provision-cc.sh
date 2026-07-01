#!/bin/sh
# tests/provision-cc.sh — behavioral test for the guix-free C-toolchain resolver
# (tools/provision-cc.sh) and its use by the td-builder SEED build
# (tools/bootstrap-td-builder.sh). Increment 2 of the guix-free daily bootstrap (github issue
# #268): after provision-rust supplies rustc/cargo (Increment 1), rustc still needs a C linker
# driver to produce the binary. Proves the seed's C source is PLUGGABLE — a PROVIDED cc (or
# the system cc on a guix-less host) is resolved in order and actually USED to build a working
# td-builder, with the pinned guix gcc-toolchain as the fallback so today's dev loop is
# unchanged. Together with tests/provision-rust.sh this shows the stage0 build needs NO guix.
set -eu
fail() { echo "FAIL: $*" >&2; exit 1; }

lock=tests/td-builder-rust.lock
test -s "$lock" || fail "no lock $lock"
lg=$(grep -- '-gcc-toolchain-' "$lock" | sed 's/^[^ ]* //' | head -1)
lr=$(grep -- '-rust-[0-9]' "$lock" | grep -v -- '-cargo' | sed 's/^[^ ]* //' | head -1)
lc=$(grep -- '-rust-.*-cargo' "$lock" | sed 's/^[^ ]* //' | head -1)
{ [ -n "$lg" ] && { [ -x "$lg/bin/gcc" ] || [ -x "$lg/bin/cc" ]; } && [ -x "$lr/bin/rustc" ] && [ -x "$lc/bin/cargo" ]; } \
  || fail "pinned lock seed not realized (run the full loop; $lg / $lr / $lc)"

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
shp=$(command -v sh); test -x "$shp" || fail "no sh on PATH"

# [resolution] 1. an explicitly PROVIDED TD_CC_HOME wins (its bin, not the lock store path).
mkdir -p "$work/pcc/bin"; : > "$work/pcc/bin/gcc"; chmod +x "$work/pcc/bin/gcc"
got=$(TD_CC_HOME="$work/pcc" sh tools/provision-cc.sh)
[ "$got" = "$work/pcc/bin" ] || fail "provided: got '$got', want $work/pcc/bin"
echo "  [resolution] a PROVIDED TD_CC_HOME toolchain is chosen ($got)"

# [resolution] 2. no TD_CC_HOME, lock present -> the pinned gcc-toolchain (dev loop preserved).
got=$(TD_CC_HOME= sh tools/provision-cc.sh)
[ "$got" = "$lg/bin" ] || fail "lock fallback: got '$got', want $lg/bin"
echo "  [resolution] no TD_CC_HOME -> the pinned lock gcc-toolchain (today's guix dev loop unchanged)"

# [resolution] 3. guix-less host shape: empty lock + a system cc on PATH.
mkdir -p "$work/sysbin"; printf '#!%s\n' "$shp" > "$work/sysbin/cc"; chmod +x "$work/sysbin/cc"
: > "$work/emptylock"
got=$(TD_CC_HOME= TD_LOCK="$work/emptylock" PATH="$work/sysbin:$PATH" sh tools/provision-cc.sh)
[ "$got" = "$work/sysbin" ] || fail "system fallback: got '$got', want $work/sysbin"
echo "  [resolution] guix-less host -> the system cc/gcc on PATH ($got)"

# [structural] the resolved PATH fragment names NO guix (a guix-free source).
case ":$(TD_CC_HOME="$work/pcc" sh tools/provision-cc.sh):" in
  *guix*) fail "the resolved C-toolchain PATH fragment names guix" ;;
esac
echo "  [structural] the resolved C-toolchain PATH fragment names no guix"

# [red] a TD_CC_HOME dir without gcc/cc is rejected (exit 3).
mkdir -p "$work/emptydir/bin"
rc=0; TD_CC_HOME="$work/emptydir" sh tools/provision-cc.sh >/dev/null 2>&1 || rc=$?
[ "$rc" = 3 ] || fail "a TD_CC_HOME without gcc/cc must exit 3 (got $rc)"
echo "  [red] a TD_CC_HOME without gcc/cc is rejected (exit 3)"

# [behavioral] the SEED build actually USES a provided Rust + provided C toolchain (both
# guix-free-named symlink dirs) to build a working td-builder — no guix on the bootpath.
mkdir -p "$work/prust/bin" "$work/pccreal/bin"
ln -s "$lr/bin/rustc" "$work/prust/bin/rustc"; ln -s "$lc/bin/cargo" "$work/prust/bin/cargo"
for f in "$lg"/bin/*; do ln -s "$f" "$work/pccreal/bin/$(basename "$f")"; done
s0=$(TD_RUST_HOME="$work/prust" TD_CC_HOME="$work/pccreal" sh tools/bootstrap-td-builder.sh "$work/out")
test -x "$s0" || fail "bootstrap produced no td-builder via the provided toolchains"
sent=$("$s0"); [ "$sent" = "td-builder 0.1.0 ok" ] || fail "the provided-toolchain td-builder sentinel was '$sent'"
printf 'provision-cc probe\n' > "$work/probe"
h=$("$s0" nar-hash "$work/probe"); [ -n "$h" ] || fail "the provided-toolchain td-builder did not nar-hash"
echo "  [behavioral] PROVIDED Rust + C toolchains built a working td-builder (sentinel + nar-hash $h)"

echo "PASS: provision-cc — the td-builder seed C toolchain is provided-or-system (guix-free), resolved in order (provided -> pinned lock -> system) and actually used (with a provided Rust toolchain) to build a working td-builder; the pinned guix gcc-toolchain stays the fallback so the dev loop is unchanged. With provision-rust this makes the stage0 build need NO guix. github issue #268 Increment 2."
