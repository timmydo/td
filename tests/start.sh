#!/usr/bin/env bash
# The checkout entry point must bootstrap the Cargo runner before `cargo run`
# can ask that runner to execute td-recipe-eval.
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
    cp "$root/.cargo/config.toml" "$1/.cargo/config.toml"
}

# A provisioned compiler without stable try_update must be diagnosed after the
# runner bootstrap and before Cargo starts building the system.
old_rust=$work/old-rust
make_fixture "$old_rust"
old_rust_log=$work/old-rust.log
set +e
PATH=$work/bin:$PATH \
TMPDIR=$work/tmp \
TD_START_TEST_FAKE_TOOLS=1 \
TD_START_TEST_LOG=$old_rust_log \
TD_START_TEST_MATERIALIZE=1 \
TD_START_TEST_RUST_PATH=$work/rust/bin \
TD_START_TEST_RUSTC_VERSION='rustc 1.92.0-nightly (test 2025-10-02)' \
TD_START_TEST_RUST_PROBE_RC=1 \
    "$old_rust/start" >/dev/null 2>"$work/old-rust.err"
old_rust_rc=$?
set -e
test "$old_rust_rc" -ne 0 || {
    echo "FAIL: start accepted Rust older than 1.95" >&2
    exit 1
}
test "$(wc -l < "$old_rust_log")" -eq 1 || {
    echo "FAIL: start reached the system Cargo run after rejecting old Rust" >&2
    exit 1
}
grep -F "Rust 1.95 or newer is required; found rustc 1.92.0-nightly" \
    "$work/old-rust.err" >/dev/null || {
    echo "FAIL: start did not diagnose the old Rust compiler" >&2
    exit 1
}

# A successful Cargo exit without the promised artifact must not reach the
# system build through an old or dangling runner.
missing=$work/missing
make_fixture "$missing"
missing_log=$work/missing.log
set +e
PATH=$work/bin:$PATH \
TMPDIR=$work/tmp \
TD_START_TEST_FAKE_TOOLS=1 \
TD_START_TEST_LOG=$missing_log \
TD_START_TEST_MATERIALIZE=0 \
    "$missing/start" >/dev/null 2>"$work/missing.err"
missing_rc=$?
set -e
test "$missing_rc" -ne 0 || {
    echo "FAIL: start accepted a build that produced no Cargo runner" >&2
    exit 1
}
test "$(wc -l < "$missing_log")" -eq 1 || {
    echo "FAIL: start reached cargo run without a built runner" >&2
    exit 1
}
grep -F "cargo did not produce an executable td-builder runner" \
    "$work/missing.err" >/dev/null || {
    echo "FAIL: start did not diagnose the missing Cargo runner" >&2
    exit 1
}

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
log=$work/cargo.log
ambient_target=$work/ambient-target
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
    "$fixture/start"

first=$(sed -n '1p' "$log")
second=$(sed -n '2p' "$log")
third=$(sed -n '3p' "$log")

IFS='|' read -r build_target_dir build_target build_home build_cwd build_args \
    <<< "$first"

test "$build_target_dir" = "$fixture/target" &&
test "$build_target" = "<unset>" &&
test "$build_cwd" = "/" &&
test "${build_home#"$ambient_tmp/td-start-cargo."}" != "$build_home" &&
test "$build_args" = "build --release --locked --manifest-path $fixture/builder/Cargo.toml" || {
    echo "FAIL: start did not build the fixed Cargo runner first: $first" >&2
    exit 1
}
test ! -e "$build_home" || {
    echo "FAIL: start left its temporary Cargo home behind: $build_home" >&2
    exit 1
}
test "$second" = "$ambient_target|wrong-target|$ambient_home|$fixture|run --release --manifest-path recipes/Cargo.toml --bin td-recipe-eval -- run system-x86-64" || {
    echo "FAIL: start did not exec the system build after bootstrapping: $second" >&2
    exit 1
}
test -z "$third" || {
    echo "FAIL: start issued an unexpected third Cargo command: $third" >&2
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

echo "PASS: start requires Rust 1.95 and bootstraps its Cargo runner"
