#!/bin/sh
# tests/provision-rust.sh — behavioral test for the guix-free Rust-toolchain resolver
# (tools/provision-rust.sh) and its use by the td-builder SEED build
# (tools/bootstrap-td-builder.sh). Increment 1 of the guix-free daily bootstrap: DESIGN.md
# §Provenance head (`rustup -> rust toolchain -> build td tools`), human 2026-07-01,
# github issue #268. Proves the seed's Rust source is now PLUGGABLE — a PROVIDED toolchain
# (or rustup on a guix-less host) is resolved in order and actually USED to build a working
# td-builder, while the pinned guix lock stays the fallback so today's dev loop is unchanged.
set -eu
fail() { echo "FAIL: $*" >&2; exit 1; }

lock=tests/td-builder-rust.lock
test -s "$lock" || fail "no lock $lock"
lr=
lc=
while IFS=' ' read -r _name _path _rest; do
  case "$_path" in
    */*-rust-[0-9]*-cargo) [ -n "$lc" ] || lc="$_path" ;;
    */*-rust-[0-9]*) [ -n "$lr" ] || lr="$_path" ;;
  esac
done < "$lock"
{ [ -n "$lr" ] && [ -x "$lr/bin/rustc" ] && [ -x "$lc/bin/cargo" ]; } \
  || fail "pinned lock Rust seed not realized (run the full loop; $lr / $lc)"

work=$(mktemp -d); trap 'rm -rf "$work"' EXIT

# A PROVIDED toolchain: a guix-free-named dir with rustc+cargo (symlinks to the realized seed).
mkdir -p "$work/provided/bin"
ln -s "$lr/bin/rustc" "$work/provided/bin/rustc"
ln -s "$lc/bin/cargo" "$work/provided/bin/cargo"

# [resolution] 1. an explicitly PROVIDED TD_RUST_HOME wins (its bin, not the lock store path).
got=$(TD_RUST_HOME="$work/provided" sh tools/provision-rust.sh)
[ "$got" = "$work/provided/bin" ] || fail "provided: got '$got', want $work/provided/bin"
echo "  [resolution] a PROVIDED TD_RUST_HOME toolchain is chosen ($got)"

# [resolution] 2. no TD_RUST_HOME, lock present -> the pinned seed (dev loop preserved).
got=$(TD_RUST_HOME= sh tools/provision-rust.sh)
case "$got" in "$lr/bin"*) : ;; *) fail "lock fallback: got '$got', want $lr/bin..." ;; esac
echo "  [resolution] no TD_RUST_HOME -> the pinned lock seed (today's guix dev loop unchanged)"

# [resolution] 3. guix-less host shape: empty lock + a rustup stub -> rustup resolves it.
# The stub's shebang must name a shell that EXISTS here — the loop sandbox has no /bin/sh —
# so resolve the running sh and use its absolute path.
shpath=$(command -v sh); test -x "$shpath" || fail "no sh on PATH to build the rustup stub"
mkdir -p "$work/stubtc/bin" "$work/stubbin"
: > "$work/stubtc/bin/rustc"; : > "$work/stubtc/bin/cargo"
chmod +x "$work/stubtc/bin/rustc" "$work/stubtc/bin/cargo"
cat > "$work/stubbin/rustup" <<EOF
#!$shpath
case "\$1" in toolchain) exit 0 ;; which) echo "$work/stubtc/bin/rustc" ;; esac
EOF
chmod +x "$work/stubbin/rustup"
: > "$work/emptylock"
got=$(TD_RUST_HOME= TD_LOCK="$work/emptylock" PATH="$work/stubbin:$PATH" sh tools/provision-rust.sh)
[ "$got" = "$work/stubtc/bin" ] || fail "rustup fallback: got '$got', want $work/stubtc/bin"
echo "  [resolution] guix-less host -> rustup installs+resolves the pinned toolchain ($got)"

# [structural] the resolved PATH fragment names NO guix (a guix-free source).
case ":$(TD_RUST_HOME="$work/provided" sh tools/provision-rust.sh):" in
  *guix*) fail "the resolved Rust PATH fragment names guix" ;;
esac
echo "  [structural] the resolved Rust PATH fragment names no guix"

# [red] a TD_RUST_HOME dir without rustc+cargo is rejected (exit 3).
mkdir -p "$work/emptydir/bin"
rc=0; TD_RUST_HOME="$work/emptydir" sh tools/provision-rust.sh >/dev/null 2>&1 || rc=$?
[ "$rc" = 3 ] || fail "a TD_RUST_HOME without rustc+cargo must exit 3 (got $rc)"
echo "  [red] a TD_RUST_HOME without rustc+cargo is rejected (exit 3)"

# [behavioral] the SEED build actually USES a provided toolchain to build a working td-builder.
s0=$(TD_RUST_HOME="$work/provided" sh tools/bootstrap-td-builder.sh "$work/out")
test -x "$s0" || fail "bootstrap produced no td-builder via the provided toolchain"
sent=$("$s0")
[ "$sent" = "td-builder 0.1.0 ok" ] || fail "the provided-toolchain td-builder sentinel was '$sent'"
printf 'provision-rust probe\n' > "$work/probe"
h=$("$s0" nar-hash "$work/probe"); [ -n "$h" ] || fail "the provided-toolchain td-builder did not nar-hash"
echo "  [behavioral] a PROVIDED Rust toolchain built a working td-builder (sentinel + nar-hash $h)"

echo "PASS: provision-rust — the td-builder seed Rust toolchain is provided-or-rustup (guix-free), resolved in order (provided -> pinned lock -> rustup) and actually used to build a working td-builder; the pinned guix lock stays the fallback so the dev loop is unchanged. DESIGN §Provenance head; github issue #268 Increment 1 (the C-linker leg is the next increment)."
