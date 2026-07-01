# tools/bootstrap-td-builder.sh — produce a STAGE0 td-builder from the checked-in
# builder/ source using ONLY a Rust toolchain — NO guix, NO Guile, NO guix-daemon. This
# breaks the bootstrap circularity at the heart of move-off-Guile §5: today the FIRST
# td-builder comes from `guix build -e '(@ (system td-builder) td-builder)'` (guix's
# cargo-build-system evaluating a Guile package), and rust-build only "self-hosts" because
# that guix-built binary already exists to run build-recipe. Here cargo compiles td-builder
# directly. td-builder has ZERO external crate deps (std-only — builder/Cargo.lock is one
# package), so the OFFLINE build needs only rustc/cargo + a gcc linker.
#
# The toolchain is provisioned guix-free (DESIGN.md §Provenance line 45: the whole userland
# bootstraps from a Rust toolchain, not guix):
#   - rustc/cargo via tools/provision-rust.sh — PROVIDED (TD_RUST_HOME) or rustup on a
#     guix-less host, else the pinned lock seed.
#   - the C linker (gcc/cc) via tools/provision-cc.sh — PROVIDED (TD_CC_HOME) or the system
#     cc on a guix-less host, else the pinned lock gcc-toolchain.
# Both fall back to the pinned lock (retired LAST §5) ONLY when its /gnu/store paths are
# present, so today's guix dev loop is byte-identical while a guix-less host uses rustup +
# system cc. td-builder is std-only with NO build script, so the link step needs ONLY those
# two — no coreutils/bash (the old bootpath carried them but the build never used them).
#
# Usage: bootstrap-td-builder.sh OUTDIR   (writes OUTDIR/bin/td-builder, prints its path)
# Env:   TD_LOCK (default tests/td-builder-rust.lock); TD_RUST_HOME / TD_RUST_VERSION
#        (provision-rust.sh); TD_CC_HOME (provision-cc.sh)
set -eu

out="${1:?usage: bootstrap-td-builder.sh OUTDIR}"
lock="${TD_LOCK:-tests/td-builder-rust.lock}"

rustpath=$(TD_LOCK="$lock" sh tools/provision-rust.sh) \
  || { echo "bootstrap: could not provision a Rust toolchain (see tools/provision-rust.sh)" >&2; exit 1; }
ccpath=$(TD_LOCK="$lock" sh tools/provision-cc.sh) \
  || { echo "bootstrap: could not provision a C toolchain (see tools/provision-cc.sh)" >&2; exit 1; }

# The bootstrap PATH carries ONLY the provisioned Rust + C toolchains — assert no guix/guile
# leaks in (the stage0 build must be guix-free, mirroring the corpus gates' scrubbed-PATH guard).
bootpath="$rustpath:$ccpath"
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
