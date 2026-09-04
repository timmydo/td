#!/usr/bin/env bash
# The checkout entry points must bootstrap the Cargo runner before `cargo run`
# can ask that runner to execute td-recipe-eval.
#
# There are two of them — `start` boots the system, `build-qcow` bundles it —
# and they share one sourced `host-preflight.sh` precisely so the bootstrap and
# the Rust floor cannot differ between them. The legs below run BOTH through
# the same fixtures: a preflight that only `start` enforces is the bug this
# file exists to prevent, and it is invisible from either script alone.
set -euo pipefail

if [[ ${TD_START_TEST_FAKE_TOOLS:-} == 1 ]]; then
    case ${0##*/} in
        cargo)
            printf '%s|%s|%s|%s|%s\n' \
                "${CARGO_TARGET_DIR-<unset>}" \
                "${CARGO_BUILD_TARGET-<unset>}" \
                "${CARGO_HOME-<unset>}" \
                "$PWD" "$*" >> "$TD_START_TEST_LOG"
            if [[ ${1-} == build && ${TD_START_TEST_MATERIALIZE:-} == 1 ]]; then
                mkdir -p "$CARGO_TARGET_DIR/release"
                printf '%s\n' \
                    '#!/bin/sh' \
                    'if [ "${1-}" = provision-rust ]; then' \
                    '    printf "%s\n" "$TD_START_TEST_RUST_PATH"' \
                    '    exit 0' \
                    'fi' \
                    'exit 1' \
                    > "$CARGO_TARGET_DIR/release/td-builder"
                chmod +x "$CARGO_TARGET_DIR/release/td-builder"
            fi
            ;;
        rustc)
            if [[ ${1-} == --version ]]; then
                printf '%s\n' "${TD_START_TEST_RUSTC_VERSION:-rustc 1.99.0}"
            else
                exit "${TD_START_TEST_RUST_PROBE_RC:-0}"
            fi
            ;;
        *) exit 1 ;;
    esac
    exit 0
fi

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/bin" "$work/tmp" "$work/rust/bin"
ln -s "$root/tests/start.sh" "$work/bin/cargo"
ln -s "$root/tests/start.sh" "$work/rust/bin/rustc"

make_fixture() {
    mkdir -p "$1/.cargo"
    cp "$root/start" "$1/start"
    cp "$root/build-qcow" "$1/build-qcow"
    cp "$root/host-preflight.sh" "$1/host-preflight.sh"
    cp "$root/.cargo/config.toml" "$1/.cargo/config.toml"
}

# One green run of an entry point under the ambient-config fixture, logging the
# Cargo calls it makes. Echoes the fixture directory it built.
run_entry_point() {
    local fixture=$1 log=$2 script=$3
    shift 3
    PATH=$work/bin:$PATH \
    TMPDIR=$ambient_tmp \
    TD_START_TEST_FAKE_TOOLS=1 \
    TD_START_TEST_LOG=$log \
    TD_START_TEST_MATERIALIZE=1 \
    TD_START_TEST_RUST_PATH=$work/rust/bin \
    TD_START_TEST_RUSTC_VERSION='rustc 1.95.0 (test)' \
    TD_START_TEST_RUST_PROBE_RC=0 \
    CARGO_TARGET_DIR=$ambient_target \
    CARGO_BUILD_TARGET=wrong-target \
    CARGO_HOME=$ambient_home \
        "$fixture/$script" "$@"
}

# The first logged Cargo call must be the isolated runner build: fixed target
# dir, no ambient build.target, a throwaway Cargo home under TMPDIR, run from
# the filesystem root against the fixture's own builder manifest.
assert_isolated_runner_build() {
    local line=$1 fixture=$2 label=$3
    local build_target_dir build_target build_home build_cwd build_args
    IFS='|' read -r build_target_dir build_target build_home build_cwd build_args \
        <<< "$line"
    test "$build_target_dir" = "$fixture/target" &&
    test "$build_target" = "<unset>" &&
    test "$build_cwd" = "/" &&
    test "${build_home#"$ambient_tmp/td-host-cargo."}" != "$build_home" &&
    test "$build_args" = "build --release --locked --manifest-path $fixture/builder/Cargo.toml" || {
        echo "FAIL: $label did not build the fixed Cargo runner first: $line" >&2
        exit 1
    }
    test ! -e "$build_home" || {
        echo "FAIL: $label left its temporary Cargo home behind: $build_home" >&2
        exit 1
    }
}

# A provisioned compiler without stable try_update must be diagnosed after the
# runner bootstrap and before Cargo starts building the system — from EITHER
# entry point. `build-qcow` runs a far longer build than `start`, so an
# unenforced floor there is the more expensive one to discover.
for script in start build-qcow; do
    old_rust=$work/old-rust-$script
    make_fixture "$old_rust"
    old_rust_log=$work/old-rust-$script.log
    : > "$old_rust_log"
    set +e
    PATH=$work/bin:$PATH \
    TMPDIR=$work/tmp \
    TD_START_TEST_FAKE_TOOLS=1 \
    TD_START_TEST_LOG=$old_rust_log \
    TD_START_TEST_MATERIALIZE=1 \
    TD_START_TEST_RUST_PATH=$work/rust/bin \
    TD_START_TEST_RUSTC_VERSION='rustc 1.92.0-nightly (test 2025-10-02)' \
    TD_START_TEST_RUST_PROBE_RC=1 \
        "$old_rust/$script" >/dev/null 2>"$work/old-rust-$script.err"
    old_rust_rc=$?
    set -e
    test "$old_rust_rc" -ne 0 || {
        echo "FAIL: $script accepted Rust older than 1.95" >&2
        exit 1
    }
    test "$(wc -l < "$old_rust_log")" -eq 1 || {
        echo "FAIL: $script reached the system Cargo run after rejecting old Rust" >&2
        exit 1
    }
    grep -F "$script: Rust 1.95 or newer is required; found rustc 1.92.0-nightly" \
        "$work/old-rust-$script.err" >/dev/null || {
        echo "FAIL: $script did not diagnose the old Rust compiler" >&2
        exit 1
    }
done

# A successful Cargo exit without the promised artifact must not reach the
# system build through an old or dangling runner.
for script in start build-qcow; do
    missing=$work/missing-$script
    make_fixture "$missing"
    missing_log=$work/missing-$script.log
    : > "$missing_log"
    set +e
    PATH=$work/bin:$PATH \
    TMPDIR=$work/tmp \
    TD_START_TEST_FAKE_TOOLS=1 \
    TD_START_TEST_LOG=$missing_log \
    TD_START_TEST_MATERIALIZE=0 \
        "$missing/$script" >/dev/null 2>"$work/missing-$script.err"
    missing_rc=$?
    set -e
    test "$missing_rc" -ne 0 || {
        echo "FAIL: $script accepted a build that produced no Cargo runner" >&2
        exit 1
    }
    test "$(wc -l < "$missing_log")" -eq 1 || {
        echo "FAIL: $script reached cargo run without a built runner" >&2
        exit 1
    }
    grep -F "$script: cargo did not produce an executable td-builder runner" \
        "$work/missing-$script.err" >/dev/null || {
        echo "FAIL: $script did not diagnose the missing Cargo runner" >&2
        exit 1
    }
done

# The green leg places TMPDIR and the fixture below an ancestor Cargo config.
# It proves the isolated runner build cannot discover that config, while the
# configured runner path still reaches the host artifact.
ambient_root=$work/ambient
ambient_tmp=$ambient_root/tmp
ambient_home=$work/ambient-home
mkdir -p "$ambient_root/.cargo" "$ambient_tmp" "$ambient_home"
printf '[build]\ntarget = "ancestor-target"\n' \
    > "$ambient_root/.cargo/config.toml"
printf '[build]\ntarget = "home-target"\n' > "$ambient_home/config.toml"
fixture=$ambient_root/repo
make_fixture "$fixture"
ambient_target=$work/ambient-target

log=$work/cargo.log
: > "$log"
run_entry_point "$fixture" "$log" start

first=$(sed -n '1p' "$log")
second=$(sed -n '2p' "$log")
third=$(sed -n '3p' "$log")

assert_isolated_runner_build "$first" "$fixture" start
test "$second" = "$ambient_target|wrong-target|$ambient_home|$fixture|run --release --manifest-path recipes/Cargo.toml --bin td-recipe-eval -- run system-x86-64" || {
    echo "FAIL: start did not exec the system build after bootstrapping: $second" >&2
    exit 1
}
test -z "$third" || {
    echo "FAIL: start issued an unexpected third Cargo command: $third" >&2
    exit 1
}

# `build-qcow` bootstraps identically and then execs the bundle command,
# forwarding its arguments verbatim rather than interpreting them.
bundle_log=$work/bundle.log
: > "$bundle_log"
run_entry_point "$fixture" "$bundle_log" build-qcow --out /tmp/somewhere --raw

bundle_first=$(sed -n '1p' "$bundle_log")
bundle_second=$(sed -n '2p' "$bundle_log")
bundle_third=$(sed -n '3p' "$bundle_log")

assert_isolated_runner_build "$bundle_first" "$fixture" build-qcow
test "$bundle_second" = "$ambient_target|wrong-target|$ambient_home|$fixture|run --release --manifest-path recipes/Cargo.toml --bin td-recipe-eval -- bundle --out /tmp/somewhere --raw" || {
    echo "FAIL: build-qcow did not exec the bundle build after bootstrapping: $bundle_second" >&2
    exit 1
}
test -z "$bundle_third" || {
    echo "FAIL: build-qcow issued an unexpected third Cargo command: $bundle_third" >&2
    exit 1
}

mapfile -t runner_paths < <(
    sed -n 's/^runner = \["\([^"]*\)", "run-capped"\]$/\1/p' \
        "$fixture/.cargo/config.toml"
)
test "${#runner_paths[@]}" -gt 0 || {
    echo "FAIL: .cargo/config.toml names no run-capped runner" >&2
    exit 1
}
runner_path=${runner_paths[0]}
for path in "${runner_paths[@]}"; do
    test "$path" = "$runner_path" || {
        echo "FAIL: Cargo target rows disagree on the runner path" >&2
        exit 1
    }
done
test -x "$fixture/$runner_path" || {
    echo "FAIL: configured runner $runner_path does not reach the host build" >&2
    exit 1
}

echo "PASS: start and build-qcow require Rust 1.95 and bootstrap their Cargo runner"
