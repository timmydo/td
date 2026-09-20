#!/usr/bin/env bash
# The ./news and ./mail entry points bootstrap the Cargo runner and hand
# everything else to `td-builder host-run NAME`: one build of the runner
# from the checkout into its own target directory, for this host, with the
# linker the host has (cc, else gcc, or TD_CC_HOME's) named for rustc when
# cc is not on PATH; then the verb with the application's name and the
# arguments as given, exec'd so the runner is the script's process and its
# exit the script's. Both scripts run through the same fake tools under a
# PATH of those alone; a bootstrap that only one of them made would be
# invisible from either alone.
set -euo pipefail

if [[ ${TD_HOST_RUN_TEST_FAKE:-} == 1 ]]; then
    case ${0##*/} in
        cargo)
            {
                printf '%s|%s|%s|%s\n' \
                    "${CARGO_BUILD_TARGET-<unset>}" \
                    "${CARGO_TARGET_DIR-<unset>}" \
                    "${CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER-<unset>}" \
                    "$PWD"
                printf '%s\n' "$PPID" "$#" "$@"
            } >> "$TD_HOST_RUN_TEST_LOG"
            # The runner the build is expected to leave behind: it logs its
            # own pid and its arguments and exits with a code the script
            # must carry.
            mkdir -p "$CARGO_TARGET_DIR/release"
            printf '%s\n' \
                '#!/bin/sh' \
                'printf "%s\n" "$$" "$#" "$@" >> "$TD_HOST_RUN_TEST_LOG"' \
                'exit 7' \
                > "$CARGO_TARGET_DIR/release/td-builder"
            chmod +x "$CARGO_TARGET_DIR/release/td-builder"
            ;;
        rustc)
            [[ $* == -vV ]] || exit 1
            printf 'rustc 1.0.0\nhost: x86_64-unknown-linux-gnu\n'
            ;;
        gcc | cc) ;;
        *) exit 1 ;;
    esac
    exit 0
fi

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/bin" "$work/fixture" "$work/toolchain/bin" "$work/decoy/fixture"
for tool in cargo rustc gcc; do
    ln -s "$root/tests/host-run.sh" "$work/bin/$tool"
done
ln -s "$root/tests/host-run.sh" "$work/toolchain/bin/gcc"
# The utilities the scripts and the fakes (this script) use, and nothing
# else of the host: no cc unless a configuration puts one there.
for tool in bash sed tr mkdir chmod; do
    ln -s "$(command -v "$tool")" "$work/bin/$tool"
done
cp "$root/news" "$root/mail" "$work/fixture/"

# One run of `script` under the configuration `label`, with the extra
# environment given, from the work directory by a relative path, under a
# CDPATH whose decoy would catch a cd that consulted it; `linker` is what
# the runner's build must name for rustc. The log is left in
# $work/$script-$label.log.
run() {
    local script=$1 label=$2 linker=$3
    shift 3
    local log=$work/$script-$label.log
    : > "$log"
    set +e
    (
        cd "$work" &&
            env -i PATH="$work/bin" HOME="$work" \
                TD_HOST_RUN_TEST_FAKE=1 TD_HOST_RUN_TEST_LOG="$log" \
                CARGO_BUILD_TARGET=wrong-target CARGO_TARGET_DIR=/elsewhere \
                CDPATH="$work/decoy" "$@" \
                "fixture/$script" --one "two three"
    ) >/dev/null 2>"$work/$script-$label.err"
    local rc=$?
    set -e
    test "$rc" -eq 7 || {
        echo "FAIL: $script ($label) did not carry the runner's exit (got $rc):" >&2
        cat "$work/$script-$label.err" >&2
        exit 1
    }
    # The build's environment and directory, the script's process (the
    # build's parent), its argument count and arguments; then the runner's
    # own pid, which is the script's process only if the script exec'd
    # it, its argument count and its arguments.
    local script_pid expected
    script_pid=$(sed -n 2p "$log")
    expected=$(printf '%s\n' \
        "<unset>|$work/fixture/target|$linker|$work/fixture" \
        "$script_pid" \
        6 build --release --locked --quiet --manifest-path builder/Cargo.toml \
        "$script_pid" 4 host-run "$script" --one "two three")
    test "$(cat "$log")" = "$expected" || {
        echo "FAIL: $script ($label): the tools were not called as expected; wanted:" >&2
        printf '%s\n' "$expected" >&2
        echo "got:" >&2
        cat "$log" >&2
        exit 1
    }
}

# A host with gcc and no cc (Guix): gcc is named as the linker.
run news guix-host gcc
run mail guix-host gcc
# A host with cc: nothing is named.
ln -s "$root/tests/host-run.sh" "$work/bin/cc"
run news cc-host '<unset>'
rm "$work/bin/cc"
# A provided toolchain: its gcc is named, whatever PATH has.
run mail provided-toolchain "$work/toolchain/bin/gcc" TD_CC_HOME="$work/toolchain"

echo "PASS: ./news and ./mail bootstrap the runner for this host and exec host-run"
