# Shared host preflight for the repository-root entry scripts.
#
# Sourced, not executed: it leaves `host_root` set and returns, and the caller
# supplies its own final `exec`. Every entry script that ends up compiling this
# checkout through cargo needs the same two guarantees before it starts, and a
# second copy of them is a second thing to forget to update.
#
# On return the temporary Cargo home is already removed and the EXIT trap is
# cleared, so a caller may `exec` immediately.

host_name=${0##*/}
host_root=$(pwd -P)

# Cargo runs this checkout's tools through its own td-builder. Build the runner
# from the filesystem root with a temporary Cargo home so ambient build.target
# settings cannot move it away from the configured path.
host_cargo_home=$(mktemp -d "${TMPDIR:-/tmp}/td-host-cargo.XXXXXX")
cleanup_host_cargo_home() {
    rm -rf -- "$host_cargo_home"
}
trap cleanup_host_cargo_home EXIT HUP INT TERM
(
    cd /
    unset CARGO_BUILD_TARGET
    CARGO_HOME=$host_cargo_home CARGO_TARGET_DIR=$host_root/target \
        cargo build --release --locked \
        --manifest-path "$host_root/builder/Cargo.toml"
)

if [ ! -x "$host_root/target/release/td-builder" ]; then
    echo "$host_name: cargo did not produce an executable td-builder runner" >&2
    exit 1
fi

# Probe the same provisioned compiler td-builder will pin for the host td-net
# build. AtomicU64::try_update became stable in Rust 1.95, and compiling the
# capability probe also handles prerelease compilers without guessing by name.
host_rust_path=$("$host_root/target/release/td-builder" provision-rust)
host_rustc=
host_saved_ifs=$IFS
IFS=:
for host_rust_dir in $host_rust_path; do
    if [ -x "$host_rust_dir/rustc" ]; then
        host_rustc=$host_rust_dir/rustc
        break
    fi
done
IFS=$host_saved_ifs
if [ -z "$host_rustc" ]; then
    echo "$host_name: provisioned Rust path contains no executable rustc: $host_rust_path" >&2
    exit 1
fi
if ! host_rust_version=$("$host_rustc" --version 2>/dev/null); then
    host_rust_version=$host_rustc
fi
if ! printf '%s\n' \
    'use std::sync::atomic::{AtomicU64, Ordering};' \
    'pub fn probe(value: &AtomicU64) {' \
    '    let _ = value.try_update(Ordering::Relaxed, Ordering::Relaxed, Some);' \
    '}' | "$host_rustc" --crate-name td_host_rust_probe --crate-type lib \
        --emit metadata -o "$host_cargo_home/td-host-rust-probe.rmeta" \
        - 2>/dev/null
then
    echo "$host_name: Rust 1.95 or newer is required; found $host_rust_version" >&2
    echo "$host_name: update the provisioned Rust toolchain and retry ./$host_name" >&2
    exit 1
fi
cleanup_host_cargo_home
trap - EXIT HUP INT TERM
