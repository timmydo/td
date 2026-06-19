# tools/bootstrap-td-builder.sh — produce a STAGE0 td-builder from the checked-in
# builder/ source using ONLY the pinned Rust toolchain — NO guix, NO Guile, NO
# guix-daemon. This breaks the bootstrap circularity at the heart of move-off-Guile §5:
# today the FIRST td-builder comes from `guix build -e '(@ (system td-builder)
# td-builder)'` (guix's cargo-build-system evaluating a Guile package), and rust-build
# only "self-hosts" because that guix-built binary already exists to run build-recipe.
# Here cargo compiles td-builder directly. td-builder has ZERO external crate deps
# (std-only — builder/Cargo.lock is one package), so the OFFLINE build needs only
# rustc/cargo + a gcc linker, all pinned in tests/td-builder-rust.lock (the guix-built
# toolchain SEED, retired LAST §5 — its store paths are read here as plain strings, no
# guix invoked to resolve or realize them).
#
# Usage: bootstrap-td-builder.sh OUTDIR   (writes OUTDIR/bin/td-builder, prints its path)
# Env:   TD_LOCK (default tests/td-builder-rust.lock)
set -eu

out="${1:?usage: bootstrap-td-builder.sh OUTDIR}"
lock="${TD_LOCK:-tests/td-builder-rust.lock}"
test -s "$lock" || { echo "bootstrap: no lock $lock" >&2; exit 1; }

# Resolve the pinned toolchain paths from the lock — grep, not guix.
rust=$(grep -- '-rust-[0-9]' "$lock" | grep -v -- '-cargo' | sed 's/^[^ ]* //' | head -1)
cargo=$(grep -- '-rust-.*-cargo' "$lock" | sed 's/^[^ ]* //' | head -1)
gcc=$(grep -- '-gcc-toolchain-' "$lock" | sed 's/^[^ ]* //' | head -1)
cu=$(grep -- '-coreutils-' "$lock" | sed 's/^[^ ]* //' | head -1)
bash=$(grep -- '-bash-' "$lock" | sed 's/^[^ ]* //' | head -1)
for p in "$rust" "$cargo" "$gcc" "$cu" "$bash"; do
  test -n "$p" || { echo "bootstrap: a toolchain path is missing from $lock" >&2; exit 1; }
  test -e "$p" || { echo "bootstrap: pinned seed not present (provision the offline toolchain, or regenerate the lock on a channel bump): $p" >&2; exit 1; }
done

# The bootstrap PATH carries ONLY pinned store tools — assert no guix/guile leaks in
# (the stage0 build must be guix-free, mirroring the corpus gates' scrubbed-PATH guard).
bootpath="$cargo/bin:$rust/bin:$gcc/bin:$cu/bin:$bash/bin"
case ":$bootpath:" in
  *guix*|*guile*) echo "bootstrap: guix/guile on the stage0 toolchain PATH — not a guix-free build" >&2; exit 1;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

# env -i: a CLEAN environment — only the pinned toolchain on PATH, nothing of the host
# (no guix, no ambient cargo/rustc). --offline --frozen: no network and Cargo.lock is
# authoritative (the std-only crate needs no registry). NIX-style determinism is not
# required here (the bootstrap binary is the SEED, re-verified bit-identical by a second
# build in the gate), but the build is in fact reproducible.
env -i PATH="$bootpath" HOME="$work" CARGO_HOME="$work/cargo" \
  cargo build --release --offline --frozen \
    --manifest-path builder/Cargo.toml --target-dir "$work/target" >&2

mkdir -p "$out/bin"
cp "$work/target/release/td-builder" "$out/bin/td-builder"
echo "$out/bin/td-builder"
