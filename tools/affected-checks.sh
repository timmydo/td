#!/usr/bin/env bash
# Select a right-sized check set from the diff against main.
#
#   tools/affected-checks.sh              # print selected checks
#   tools/affected-checks.sh --run        # execute selected checks
#   tools/affected-checks.sh --base main  # compare against another base
#   tools/affected-checks.sh --path FILE  # inspect the mapping for FILE
#   tools/affected-checks.sh --self-test  # verify the mapping table
#
# This is the local PR-readiness gate for diffs it can classify. It maps
# changed paths to focused Make targets and prints whether the full ./check.sh
# is waived or still required.
set -euo pipefail
cd "$(dirname "$0")/.."

base=origin/main
run=0
committed_only=0
self_test=0
explicit_paths=()

usage() {
  awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "$0"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --run) run=1 ;;
    --self-test) self_test=1 ;;
    --base)
      shift
      [ "$#" -gt 0 ] || { echo "affected-checks: --base needs a ref" >&2; exit 2; }
      base=$1
      ;;
    --committed-only) committed_only=1 ;;
    --path)
      shift
      [ "$#" -gt 0 ] || { echo "affected-checks: --path needs a path" >&2; exit 2; }
      explicit_paths+=("$1")
      ;;
    -h|--help) usage; exit 0 ;;
    *) echo "affected-checks: unknown arg '$1'" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

preflights=()
targets=()
notes=()
full_required=()

contains_word() { # $1 = needle, rest = haystack words
  local n=$1; shift
  local x
  for x in "$@"; do [ "$x" = "$n" ] && return 0; done
  return 1
}

add_preflight() {
  contains_word "$1" "${preflights[@]}" || preflights+=("$1")
}

add_target() {
  contains_word "$1" "${targets[@]}" || targets+=("$1")
}

add_note() {
  contains_word "$1" "${notes[@]}" || notes+=("$1")
}

require_full() {
  contains_word "$1" "${full_required[@]}" || full_required+=("$1")
}

target_from_gate_file() {
  sed -n 's/^\(CHEAP_GATES\|HEAVY_GATES\|FAST_GATES\|SYSTEM_GATES\)[[:space:]]*+=[[:space:]]*//p' "$1" | head -n1
}

add_gate_file_targets() {
  add_target "$1"
  case "$1" in
    offline)
      # The old Guix oracle and td's own offline builder enforce the same durable
      # isolation property; edits to either side of the shared offline probe need both.
      add_target td-offline ;;
  esac
}

add_build_gate_targets() {
  local gates gate
  add_target build-recipes
  gates=$(sed -n 's/^BUILD_GATES[[:space:]]*+=[[:space:]]*//p' mk/gates/*.mk)
  for gate in $gates; do
    add_target "$gate"
  done
}

target_for_build_spec() {
  local spec=$1 file target specs
  for file in mk/gates/*.mk; do
    [ -f "$file" ] || continue
    target=$(target_from_gate_file "$file" || true)
    [ -n "$target" ] || continue
    specs=$(sed -n 's/^[A-Za-z0-9_-]*_SPECS[[:space:]]*:=[[:space:]]*//p' "$file")
    if contains_word "$spec" $specs; then
      echo "$target"
      return 0
    fi
  done
  return 1
}

default_check_covers_target() {
  local target=$1 gate
  case "$target" in
    check-fast|build-recipes)
      return 0 ;;
  esac

  for gate in $(sed -n 's/^\(CHEAP_GATES\|HEAVY_GATES\)[[:space:]]*+=[[:space:]]*//p' mk/gates/*.mk); do
    [ "$gate" = "$target" ] && return 0
  done
  return 1
}

map_recipe_spec() {
  local target
  if target=$(target_for_build_spec "$1"); then
    add_target "$target"
    return
  fi

  case "$1" in
    td-builder)
      add_target rust-build ;;
    td-vendor-demo)
      add_target rust-vendor ;;
    td-russh-demo)
      add_target rust-russh ;;
    td-cmake-demo)
      add_target cmake ;;
    cat)
      add_target rust-uutils ;;
    eza)
      add_target rust-eza ;;
    bat)
      add_target rust-bat ;;
    sd)
      add_target rust-sd ;;
    procs)
      add_target rust-procs ;;
    fd)
      add_target rust-fd ;;
    ripgrep)
      add_target rust-ripgrep ;;
    uutils)
      add_target rust-coreutils ;;
    youki)
      add_target rust-youki ;;
    td-fetch)
      add_target rust-fetch ;;
    td-feed)
      add_target td-feed ;;
    td-subst)
      add_target td-subst ;;
    perturbed)
      add_target drv-emit ;;
    pkg-config)
      add_target guix-dependence
      add_note "pkg-config is authored but excluded from td-built census until it has an own-builder gate." ;;
    *)
      add_target check-fast
      require_full "No recipe-specific mapping for '$1'; update affected-checks.sh or run full ./check.sh." ;;
  esac
}

map_path() {
  local p=$1 spec gate
  case "$p" in
    .claude/*|.td-build-cache/*|builder/target/*)
      return 0 ;;

    Makefile|check.sh)
      add_preflight shell-syntax
      add_target check-fast
      add_target cargo-test
      require_full "$p touches the loop spine; affected-checks cannot waive the full loop." ;;

    mk/gates/*.mk)
      add_preflight shell-syntax
      add_preflight affected-self-test
      if [ -f "$p" ]; then
        gate=$(target_from_gate_file "$p" || true)
        if [ -n "$gate" ]; then
          add_gate_file_targets "$gate"
        else
          add_target check-fast
          require_full "$p does not register a gate target; update the gate or run full ./check.sh."
        fi
      else
        add_target check-fast
        require_full "$p was deleted or is unavailable; affected-checks cannot infer the removed gate target."
      fi ;;

    builder/Cargo.toml|builder/Cargo.lock|builder/src/*)
      # The td-builder build engine (realize_drv/build_recipe/sandbox/store/drv/nar …) is
      # the spine of every recipe-building gate. The full heavy+system suite is NO LONGER a
      # per-PR blocking gate (DESIGN §7.2, human 2026-06-21: it runs DAILY as an
      # agent-driven backstop that opens a fix-or-revert PR on regression). So an engine
      # diff validates locally on the `check-engine` SMOKE tier — a TRUE ~2-min smoke:
      # cheap structural gates + `cargo-test` (compile the engine + its drv/store/NAR/scan/
      # sandbox unit tests), and NOTHING that builds a package from source. The end-to-end
      # build coverage (bootstrap-build/build-plan/td-check/corpus/repro) is the DAILY
      # backstop, not blocked here (the accepted velocity trade). cargo-test also runs as a
      # host preflight for fast-fail.
      add_preflight cargo-test
      add_target check-engine
      add_note "$p is the td-builder build engine: validated by the ~2-min check-engine smoke (compile + unit tests); the from-source build coverage is the DAILY backstop (DESIGN §7.2), not a per-PR gate." ;;

    ts-eval/*|ts-eval/src/*|ts-eval/Cargo.toml|ts-eval/Cargo.lock)
      add_target ts-eval
      add_target ts-diff ;;

    fetch/*|fetch/src/*|fetch/Cargo.toml|fetch/Cargo.lock)
      add_target rust-fetch ;;

    feed/*|feed/src/*|feed/Cargo.toml|feed/Cargo.lock)
      add_target td-feed ;;

    subst/*|subst/src/*|subst/Cargo.toml|subst/Cargo.lock)
      add_target td-subst ;;

    tests/td-tsgo.lock|tests/tsgo.sh|tools/warm-tsgo.sh)
      add_preflight shell-syntax
      add_target tsgo-pin
      add_target ts ;;

    tests/toolchain-input-addressed.sh)
      add_preflight shell-syntax
      add_target toolchain-input-addressed ;;

    tests/td-toolchain-x86_64.lock)
      # the x86_64 toolchain's input-addressed key feeds BOTH the addressing gate (418, #219)
      # and gate 414, which interns the real x86_64 bytes at this lock's path and runs from it.
      add_preflight shell-syntax
      add_target toolchain-x86_64-input-addressed
      add_target bootstrap-x86_64-toolchain-store-native ;;

    tests/toolchain-x86_64-input-addressed.sh|mk/gates/418-toolchain-x86_64-input-addressed.mk)
      add_preflight shell-syntax
      add_target toolchain-x86_64-input-addressed ;;

    tests/td-toolchain.lock)
      # the lock keys BOTH the input-addressed path (2a) and the default substitute (this track);
      # the x86_64 gate also reads it for its arch-parity (same source set) leg.
      add_preflight shell-syntax
      add_target toolchain-input-addressed
      add_target toolchain-subst-default
      add_target toolchain-x86_64-input-addressed ;;

    tests/toolchain-subst-default.sh|tools/resolve-toolchain.sh|tools/publish-toolchain-subst.sh|tests/td-subst.pub)
      add_preflight shell-syntax
      add_target toolchain-subst-default ;;

    tests/ts/recipe-*-perturbed.ts)
      spec=${p##*/recipe-}
      spec=${spec%-perturbed.ts}
      map_recipe_spec "$spec" ;;

    tests/ts/recipe-*.ts)
      spec=${p##*/recipe-}
      spec=${spec%.ts}
      map_recipe_spec "$spec" ;;

    tests/ts/spec-*.ts|tests/ts/td-spec.d.ts|tests/ts/spec-v0.expected.js)
      add_target ts
      add_target ts-diff ;;

    tests/*-no-guix.lock)
      spec=${p##tests/}
      spec=${spec%-no-guix.lock}
      map_recipe_spec "$spec" ;;

    tests/td-builder-rust.lock|tests/td-builder-source.scm)
      add_target rust-build ;;

    tests/td-vendor-demo.lock|tests/td-vendor-demo-source.scm|tests/vendor-demo/*|tests/vendor-demo/src/*)
      add_target rust-vendor ;;

    tests/td-russh-demo.lock|tests/td-russh-demo-source.scm|tests/russh-demo/*)
      add_target rust-russh ;;

    tests/td-cmake-demo.lock|tests/cmake-demo/*)
      add_target cmake ;;

    tests/cat-uutils.lock)
      add_target rust-uutils ;;

    tests/eza.lock)
      add_target rust-eza ;;

    tests/bat.lock)
      add_target rust-bat ;;

    tests/sd.lock)
      add_target rust-sd ;;
    tests/procs.lock)
      add_target rust-procs ;;

    tests/fd.lock)
      add_target rust-fd ;;
    tests/ripgrep.lock)
      add_target rust-ripgrep ;;

    tests/uutils-coreutils.lock)
      add_target rust-coreutils ;;

    tests/youki.lock)
      add_target rust-youki ;;

    tests/td-fetch.lock)
      add_target rust-fetch ;;

    tests/td-feed.lock|tests/td-feed.index)
      add_target td-feed ;;

    tests/td-subst.lock)
      add_target td-subst ;;

    tools/gen-feed-index.sh)
      add_preflight shell-syntax
      add_target td-feed ;;

    tools/feed-ensure.sh)
      add_preflight shell-syntax
      add_target feed-shared ;;

    tools/warm-td-fetch-crates.sh)
      add_preflight shell-syntax
      add_target rust-fetch
      add_target td-feed ;;

    tools/warm-cargo-proxy.sh|tests/crate-free-build.sh)
      add_preflight shell-syntax
      add_target rust-ripgrep
      add_target rust-sd
      add_target rust-fd
      add_target rust-procs
      add_target rust-eza
      add_target rust-bat
      add_target rust-coreutils
      add_target rust-uutils
      add_target rust-youki ;;

    tools/warm-cargo-proxy-local.sh)
      add_preflight shell-syntax
      add_target rust-russh ;;

    tests/build-pkg.sh|tests/cache-lib.sh|tests/stage0-builder.sh)
      add_preflight shell-syntax
      add_build_gate_targets ;;

    tests/check-memo*)
      add_target memo ;;

    tests/td-builder-nar.scm|tests/td-builder-s3-drvs.scm|tests/td-builder-s4-drv.scm)
      add_target td-builder ;;

    tests/drv-emit-drv.scm)
      add_target drv-emit ;;
    tests/td-drv-build-drv.scm)
      add_target td-drv-build ;;
    tests/td-drv-add-drv.scm)
      add_target td-drv-add ;;
    tests/td-drv-assemble-drv.scm)
      add_target td-drv-assemble ;;
    tests/resolve-lock.scm)
      add_target resolve ;;

    tests/rootless*)
      add_preflight shell-syntax
      add_target rootless ;;

    tests/offline-drv.scm)
      add_target offline
      add_target td-offline ;;

    tests/sandbox-hardening.sh)
      add_preflight shell-syntax
      add_target sandbox-hardening ;;

    tests/td-shell.sh)
      add_preflight shell-syntax
      add_target td-shell ;;

    tests/td-shell-seed.sh)
      add_preflight shell-syntax
      add_target td-shell-seed ;;

    tests/profile.sh)
      add_preflight shell-syntax
      add_target profile ;;

    tests/bootstrap-seed.sh)
      add_preflight shell-syntax
      add_target bootstrap-seed ;;

    tests/bootstrap-cc.sh)
      add_preflight shell-syntax
      add_target bootstrap-cc ;;

    tests/bootstrap-mes.sh)
      add_preflight shell-syntax
      add_target bootstrap-mes ;;

    tests/bootstrap-mescc.sh)
      add_preflight shell-syntax
      add_target bootstrap-mescc ;;

    tests/bootstrap-tcc.sh)
      add_preflight shell-syntax
      add_target bootstrap-tcc ;;

    tests/bootstrap-make.sh)
      add_preflight shell-syntax
      add_target bootstrap-make ;;

    tests/bootstrap-tools.sh|seed/sources/gzip-*.lock|seed/sources/tcc-0.9.27*.lock)
      add_preflight shell-syntax
      add_target bootstrap-tools ;;

    tests/rust-store-native.sh|seed/sources/rust-*.lock)
      add_preflight shell-syntax
      add_target rust-store-native
      add_target rust-x86_64-runtime-store-native ;;

    tests/rust-x86_64-runtime-store-native.sh|seed/sources/zlib-*.lock|mk/gates/416-rust-x86_64-runtime-store-native.mk)
      add_preflight shell-syntax
      add_target rust-x86_64-runtime-store-native ;;

    tests/bootstrap-patch.sh|seed/sources/patch-*.lock)
      add_preflight shell-syntax
      add_target bootstrap-patch
      add_target bootstrap-binutils
      add_target bootstrap-gcc
      add_target bootstrap-glibc
      add_target bootstrap-gcc-mesboot0
      add_target bootstrap-binutils-mesboot1
      add_target bootstrap-make-mesboot
      add_target bootstrap-gcc-core-mesboot1
      add_target bootstrap-gcc-mesboot1
      add_target bootstrap-binutils-gawk-mesboot
      add_target bootstrap-glibc-mesboot
      add_target bootstrap-gcc-mesboot
      add_target bootstrap-toolchain-store-native
      add_target bootstrap-glibc-shared-store-native
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    tests/bootstrap-binutils.sh|seed/sources/binutils-2.20*.lock|seed/patches/binutils-boot-*.patch)
      add_preflight shell-syntax
      add_target bootstrap-binutils
      add_target bootstrap-gcc
      add_target bootstrap-glibc
      add_target bootstrap-gcc-mesboot0
      add_target bootstrap-binutils-mesboot1
      add_target bootstrap-make-mesboot
      add_target bootstrap-gcc-core-mesboot1
      add_target bootstrap-gcc-mesboot1
      add_target bootstrap-binutils-gawk-mesboot
      add_target bootstrap-glibc-mesboot
      add_target bootstrap-gcc-mesboot
      add_target bootstrap-toolchain-store-native
      add_target bootstrap-glibc-shared-store-native
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    tests/bootstrap-gcc.sh|seed/sources/gcc-core-2.95.3.lock|seed/patches/gcc-boot-2.95.3.patch)
      add_preflight shell-syntax
      add_target bootstrap-gcc
      add_target bootstrap-glibc
      add_target bootstrap-gcc-mesboot0
      add_target bootstrap-binutils-mesboot1
      add_target bootstrap-make-mesboot
      add_target bootstrap-gcc-core-mesboot1
      add_target bootstrap-gcc-mesboot1
      add_target bootstrap-binutils-gawk-mesboot
      add_target bootstrap-glibc-mesboot
      add_target bootstrap-gcc-mesboot
      add_target bootstrap-toolchain-store-native
      add_target bootstrap-glibc-shared-store-native
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    tests/bootstrap-glibc.sh|seed/sources/glibc-2.2.5.lock|seed/sources/linux-*.lock|seed/patches/glibc-boot-2.2.5.patch|seed/patches/glibc-bootstrap-system-2.2.5.patch|tools/warm-kernel-headers.sh)
      add_preflight shell-syntax
      add_target bootstrap-glibc
      add_target bootstrap-gcc-mesboot0
      add_target bootstrap-binutils-mesboot1
      add_target bootstrap-make-mesboot
      add_target bootstrap-gcc-core-mesboot1
      add_target bootstrap-gcc-mesboot1
      add_target bootstrap-binutils-gawk-mesboot
      add_target bootstrap-glibc-mesboot
      add_target bootstrap-gcc-mesboot
      add_target bootstrap-toolchain-store-native
      add_target bootstrap-glibc-shared-store-native
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    tests/bootstrap-gcc-mesboot0.sh)
      add_preflight shell-syntax
      add_target bootstrap-gcc-mesboot0 ;;

    tests/bootstrap-binutils-mesboot1.sh)
      add_preflight shell-syntax
      add_target bootstrap-binutils-mesboot1 ;;

    tests/bootstrap-make-mesboot.sh|seed/sources/make-3.82.lock)
      add_preflight shell-syntax
      add_target bootstrap-make-mesboot ;;

    tests/bootstrap-gcc-core-mesboot1.sh|seed/sources/gcc-core-4.6.4.lock|seed/sources/gmp-*.lock|seed/sources/mpfr-*.lock|seed/sources/mpc-*.lock|seed/patches/gcc-boot-4.6.4.patch)
      add_preflight shell-syntax
      add_target bootstrap-gcc-core-mesboot1
      add_target bootstrap-gcc-mesboot1
      add_target bootstrap-binutils-gawk-mesboot
      add_target bootstrap-glibc-mesboot
      add_target bootstrap-gcc-mesboot
      add_target bootstrap-toolchain-store-native
      add_target bootstrap-glibc-shared-store-native
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    tests/bootstrap-gcc-mesboot1.sh|seed/sources/gcc-g++-4.6.4.lock)
      add_preflight shell-syntax
      add_target bootstrap-gcc-mesboot1
      add_target bootstrap-binutils-gawk-mesboot
      add_target bootstrap-glibc-mesboot
      add_target bootstrap-gcc-mesboot
      add_target bootstrap-toolchain-store-native
      add_target bootstrap-glibc-shared-store-native
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    tests/bootstrap-binutils-gawk-mesboot.sh|seed/sources/gawk-*.lock)
      add_preflight shell-syntax
      add_target bootstrap-binutils-gawk-mesboot
      add_target bootstrap-glibc-mesboot
      add_target bootstrap-gcc-mesboot
      add_target bootstrap-toolchain-store-native
      add_target bootstrap-glibc-shared-store-native
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    tests/bootstrap-glibc-mesboot.sh|seed/sources/glibc-mesboot-2.16.0.lock|seed/patches/glibc-boot-2.16.0.patch|seed/patches/glibc-bootstrap-system-2.16.0.patch)
      add_preflight shell-syntax
      add_target bootstrap-glibc-mesboot
      add_target bootstrap-gcc-mesboot
      add_target bootstrap-toolchain-store-native
      add_target bootstrap-glibc-shared-store-native
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    tests/bootstrap-gcc-mesboot.sh|seed/sources/gcc-4.9.4.lock)
      add_preflight shell-syntax
      add_target bootstrap-gcc-mesboot
      add_target bootstrap-toolchain-store-native
      add_target bootstrap-glibc-shared-store-native
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    tests/bootstrap-toolchain-store-native.sh)
      add_preflight shell-syntax
      add_target bootstrap-toolchain-store-native
      add_target bootstrap-glibc-shared-store-native
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native ;;

    tests/bootstrap-glibc-shared-store-native.sh)
      add_preflight shell-syntax
      add_target bootstrap-glibc-shared-store-native
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native ;;

    tests/bootstrap-gcc-mesboot-wrapper.sh)
      add_preflight shell-syntax
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native ;;

    tests/bootstrap-hello-userland.sh|seed/sources/hello-2.10.lock)
      add_preflight shell-syntax
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native ;;

    tests/repro-lib.sh)
      # shared reproducibility normalization (strip debug + deterministic archives + drop .la)
      # sourced by the modern store-native rungs — exercise the rungs that use/adopt it.
      add_preflight shell-syntax
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native ;;

    tests/bootstrap-binutils-244-store-native.sh|seed/sources/binutils-2.44.lock)
      add_preflight shell-syntax
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    tests/bootstrap-gcc-mesboot-494-store-native.sh)
      add_preflight shell-syntax
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native ;;

    tests/bootstrap-gcc-14-store-native.sh|seed/sources/gcc-14.3.0.lock|seed/sources/gcc14-*.lock)
      add_preflight shell-syntax
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    tests/bootstrap-glibc-241-store-native.sh|seed/sources/glibc-2.41.lock)
      add_preflight shell-syntax
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    tests/bootstrap-hello-corpus-store-native.sh|seed/sources/hello-2.12.2.lock)
      add_preflight shell-syntax
      add_target bootstrap-hello-corpus-store-native ;;

    tests/bootstrap-x86_64-toolchain-store-native.sh|tests/x86_64-cross-fns.sh|tools/warm-kernel-headers-x86_64.sh|mk/gates/414-bootstrap-x86_64-toolchain-store-native.mk)
      add_preflight shell-syntax
      add_target bootstrap-x86_64-toolchain-store-native ;;

    seed/sources/make-*.lock)
      add_preflight shell-syntax
      add_target bootstrap-make
      add_target bootstrap-tools
      add_target bootstrap-patch
      add_target bootstrap-binutils
      add_target bootstrap-gcc
      add_target bootstrap-glibc
      add_target bootstrap-gcc-mesboot0
      add_target bootstrap-binutils-mesboot1
      add_target bootstrap-make-mesboot
      add_target bootstrap-gcc-core-mesboot1
      add_target bootstrap-gcc-mesboot1
      add_target bootstrap-binutils-gawk-mesboot
      add_target bootstrap-glibc-mesboot
      add_target bootstrap-gcc-mesboot
      add_target bootstrap-toolchain-store-native
      add_target bootstrap-glibc-shared-store-native
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    seed/sources/tcc-0.9.26*.lock)
      add_preflight shell-syntax
      add_target bootstrap-tcc
      add_target bootstrap-make
      add_target bootstrap-tools
      add_target bootstrap-patch
      add_target bootstrap-binutils
      add_target bootstrap-gcc
      add_target bootstrap-glibc
      add_target bootstrap-gcc-mesboot0
      add_target bootstrap-binutils-mesboot1
      add_target bootstrap-make-mesboot
      add_target bootstrap-gcc-core-mesboot1
      add_target bootstrap-gcc-mesboot1
      add_target bootstrap-binutils-gawk-mesboot
      add_target bootstrap-glibc-mesboot
      add_target bootstrap-gcc-mesboot
      add_target bootstrap-toolchain-store-native
      add_target bootstrap-glibc-shared-store-native
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    seed/sources/nyacc-*.lock)
      add_preflight shell-syntax
      add_target bootstrap-mescc
      add_target bootstrap-tcc
      add_target bootstrap-make
      add_target bootstrap-tools
      add_target bootstrap-patch
      add_target bootstrap-binutils
      add_target bootstrap-gcc
      add_target bootstrap-glibc
      add_target bootstrap-gcc-mesboot0
      add_target bootstrap-binutils-mesboot1
      add_target bootstrap-make-mesboot
      add_target bootstrap-gcc-core-mesboot1
      add_target bootstrap-gcc-mesboot1
      add_target bootstrap-binutils-gawk-mesboot
      add_target bootstrap-glibc-mesboot
      add_target bootstrap-gcc-mesboot
      add_target bootstrap-toolchain-store-native
      add_target bootstrap-glibc-shared-store-native
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    seed/sources/mes-*.lock|tools/warm-bootstrap-sources.sh)
      add_preflight shell-syntax
      add_target bootstrap-mes
      add_target bootstrap-mescc
      add_target bootstrap-tcc
      add_target bootstrap-make
      add_target bootstrap-tools
      add_target bootstrap-patch
      add_target bootstrap-binutils
      add_target bootstrap-gcc
      add_target bootstrap-glibc
      add_target bootstrap-gcc-mesboot0
      add_target bootstrap-binutils-mesboot1
      add_target bootstrap-make-mesboot
      add_target bootstrap-gcc-core-mesboot1
      add_target bootstrap-gcc-mesboot1
      add_target bootstrap-binutils-gawk-mesboot
      add_target bootstrap-glibc-mesboot
      add_target bootstrap-gcc-mesboot
      add_target bootstrap-toolchain-store-native
      add_target bootstrap-glibc-shared-store-native
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    seed/stage0/*)
      add_preflight shell-syntax
      add_target bootstrap-seed
      add_target bootstrap-cc
      add_target bootstrap-mes
      add_target bootstrap-mescc
      add_target bootstrap-tcc
      add_target bootstrap-make
      add_target bootstrap-tools
      add_target bootstrap-patch
      add_target bootstrap-binutils
      add_target bootstrap-gcc
      add_target bootstrap-glibc
      add_target bootstrap-gcc-mesboot0
      add_target bootstrap-binutils-mesboot1
      add_target bootstrap-make-mesboot
      add_target bootstrap-gcc-core-mesboot1
      add_target bootstrap-gcc-mesboot1
      add_target bootstrap-binutils-gawk-mesboot
      add_target bootstrap-glibc-mesboot
      add_target bootstrap-gcc-mesboot
      add_target bootstrap-toolchain-store-native
      add_target bootstrap-glibc-shared-store-native
      add_target bootstrap-gcc-mesboot-wrapper
      add_target bootstrap-hello-userland
      add_target bootstrap-binutils-244-store-native
      add_target bootstrap-gcc-mesboot-494-store-native
      add_target bootstrap-gcc-14-store-native
      add_target bootstrap-glibc-241-store-native
      add_target bootstrap-hello-corpus-store-native
      add_target bootstrap-x86_64-toolchain-store-native ;;

    tests/store-ns.sh)
      add_preflight shell-syntax
      add_target store-ns ;;

    tests/store-relocate.sh)
      add_preflight shell-syntax
      add_target store-relocate ;;

    tests/store-native-profile.sh)
      add_preflight shell-syntax
      add_target store-native-profile ;;

    tests/seed-tarball.sh|tools/build-seed-tarball.sh)
      add_preflight shell-syntax
      add_target seed-tarball ;;

    tests/seed-unpack.sh)
      add_preflight shell-syntax
      add_target seed-unpack ;;

    tests/seed-build.sh|tools/warm-seed.sh|tests/td-seed.lock)
      add_preflight shell-syntax
      add_target seed-build ;;

    tests/corpus-seed.sh)
      add_preflight shell-syntax
      add_target corpus-seed ;;

    tests/rust-seed.sh)
      add_preflight shell-syntax
      add_target rust-seed ;;

    tests/harness-seed.sh)
      add_preflight shell-syntax
      add_target harness-seed ;;

    tests/guix-dependence.*)
      add_target guix-dependence ;;

    tests/guix-surface.*)
      add_preflight shell-syntax
      add_target guix-surface ;;

    tests/ts-emit.sh|tests/ts-check.sh)
      add_preflight shell-syntax
      add_target ts
      add_target ts-diff ;;

    tests/ts-eval-check.sh)
      add_preflight shell-syntax
      add_target ts-eval ;;

    system/td-builder.scm)
      add_target td-builder
      add_target rust-build ;;

    system/td-ts.scm)
      add_target ts
      add_target ts-eval
      add_target ts-diff ;;

    system/td.scm)
      add_preflight shell-syntax
      add_target check-system
      require_full "$p is exclusive landing spine; coordinate the landing and run the full local loop." ;;

    system/*|tests/boot*|tests/reset*|tests/vm-lib.sh|tests/container.scm|tests/run-image.sh|tests/rollback*|tests/place*|tests/verify-place*|tests/registry*|tests/manifest*|tests/generation*|tests/oci*)
      add_preflight shell-syntax
      add_target check-system ;;

    PLAN.md|plan/tracks/*|tools/plan-index.sh)
      add_preflight plan-index ;;

    tools/affected-checks.sh)
      add_preflight shell-syntax
      add_preflight affected-self-test ;;

    tests/heal-revert.sh)
      # CI-lint-only behavioral test of ci/revert-suspect.sh (the heal
      # primitive). git is absent from the loop sandbox, so it is NOT a
      # ./check.sh gate — it runs in ci.yml's lint job. Shell-syntax suffices
      # for local readiness; the lint job runs the test itself.
      add_preflight shell-syntax ;;

    ci/build-ci-image.sh|ci/import-store.sh|ci/lower-*.sh|.github/setup-branch-protection.sh|.github/workflows/*)
      add_preflight shell-syntax
      require_full "$p affects CI or runner gating; affected-checks cannot waive the full local loop."
      add_note "$p affects CI or branch protection; inspect the workflow result after push." ;;

    ci/*.sh|tools/*.sh)
      add_preflight shell-syntax ;;

    ci/*|.github/workflows/*|.github/*)
      add_preflight shell-syntax
      add_note "$p affects CI or branch protection; inspect the workflow result after push." ;;

    DIGESTS.md)
      require_full "$p is exclusive landing spine; coordinate the landing and run the full local loop." ;;

    *.md|plan/*|HISTORY.md|DESIGN.md|CLAUDE.md|DIGESTS.md)
      : ;;

    channels.scm)
      add_target check-fast
      add_target guix-dependence
      require_full "channels.scm changed; the dependency pin affects the whole loop." ;;

    *)
      add_target check-fast
      require_full "No mapping for $p; update affected-checks.sh or run full ./check.sh." ;;
  esac
}

run_self_test() {
  local failures=0 out file gate specs spec build_gates build_gate

  fail() {
    echo "FAIL: $*" >&2
    failures=$((failures + 1))
  }

  assert_contains() {
    local haystack=$1 needle=$2 label=$3
    case "$haystack" in
      *"$needle"*) : ;;
      *) fail "$label: missing '$needle'" ;;
    esac
  }

  assert_not_contains() {
    local haystack=$1 needle=$2 label=$3
    case "$haystack" in
      *"$needle"*) fail "$label: unexpectedly contains '$needle'" ;;
      *) : ;;
    esac
  }

  path_output() {
    "$0" --path "$1"
  }

  output_has_target() {
    local haystack=$1 target=$2 line
    line=$(printf '%s\n' "$haystack" | sed -n 's/^  \.\/check\.sh //p' | tail -n1)
    contains_word "$target" $line
  }

  assert_target() {
    local path=$1 target=$2
    out=$(path_output "$path") || { fail "$path: dry-run failed"; return; }
    if ! output_has_target "$out" "$target"; then
      fail "$path: expected ./check.sh target '$target'"
    fi
  }

  assert_preflight() {
    local path=$1 text=$2
    out=$(path_output "$path") || { fail "$path: dry-run failed"; return; }
    assert_contains "$out" "$text" "$path"
  }

  assert_branch_policy() {
    local path=$1 policy=$2
    out=$(path_output "$path") || { fail "$path: dry-run failed"; return; }
    assert_contains "$out" "Branch-mode policy for these paths: $policy" "$path"
  }

  out=$("$0" --help) || { fail "--help failed"; out=; }
  assert_contains "$out" "--self-test" "--help"
  assert_not_contains "$out" "set -euo pipefail" "--help"
  assert_not_contains "$out" 'cd "$(dirname "$0")/.."' "--help"

  default_check_covers_target check-fast || fail "default coverage: missing check-fast"
  default_check_covers_target build-recipes || fail "default coverage: missing build-recipes"
  default_check_covers_target cargo-test || fail "default coverage: missing cargo-test"
  if default_check_covers_target check-system; then
    fail "default coverage: check-system is not covered by plain ./check.sh"
  fi
  if default_check_covers_target oci-diff; then
    fail "default coverage: system gate oci-diff is not covered by plain ./check.sh"
  fi

  for file in mk/gates/*.mk; do
    [ -f "$file" ] || continue
    gate=$(target_from_gate_file "$file" || true)
    if [ -z "$gate" ]; then
      fail "$file: no gate registration found"
      continue
    fi
    assert_target "$file" "$gate"
  done

  if [ -f mk/gates/185-offline.mk ]; then
    assert_target mk/gates/185-offline.mk offline
    assert_target mk/gates/185-offline.mk td-offline
  fi

  build_gates=$(sed -n 's/^BUILD_GATES[[:space:]]*+=[[:space:]]*//p' mk/gates/*.mk)
  for build_gate in $build_gates; do
    assert_target tests/build-pkg.sh "$build_gate"
    assert_target tests/cache-lib.sh "$build_gate"
  done

  for file in mk/gates/*.mk; do
    [ -f "$file" ] || continue
    gate=$(target_from_gate_file "$file" || true)
    [ -n "$gate" ] || continue
    specs=$(sed -n 's/^[A-Za-z0-9_-]*_SPECS[[:space:]]*:=[[:space:]]*//p' "$file")
    for spec in $specs; do
      if [ -f "tests/ts/recipe-$spec.ts" ]; then
        assert_target "tests/ts/recipe-$spec.ts" "$gate"
      fi
      if [ -f "tests/$spec-no-guix.lock" ]; then
        assert_target "tests/$spec-no-guix.lock" "$gate"
      fi
    done
  done

  assert_preflight tools/affected-checks.sh "tools/affected-checks.sh --self-test"
  assert_branch_policy tools/affected-checks.sh "full ./check.sh would be waived"
  assert_target tests/repro-lib.sh bootstrap-binutils-244-store-native
  assert_branch_policy tests/repro-lib.sh "full ./check.sh would be waived"
  assert_target tests/ts/recipe-td-russh-demo.ts rust-russh
  assert_target tests/td-russh-demo.lock rust-russh
  assert_target tests/russh-demo/Cargo.lock rust-russh
  assert_target tools/warm-cargo-proxy-local.sh rust-russh
  assert_target tests/ts/recipe-td-feed.ts td-feed
  assert_target tests/td-feed.lock td-feed
  assert_target tests/td-feed.index td-feed
  assert_target tools/feed-ensure.sh feed-shared
  assert_target tools/warm-td-fetch-crates.sh rust-fetch
  assert_target tools/warm-td-fetch-crates.sh td-feed
  assert_target tools/warm-cargo-proxy.sh rust-ripgrep
  assert_target feed/src/main.rs td-feed
  assert_target tests/ts/recipe-td-subst.ts td-subst
  assert_target tests/td-subst.lock td-subst
  assert_target subst/src/main.rs td-subst
  assert_target tests/toolchain-subst-default.sh toolchain-subst-default
  assert_target tools/resolve-toolchain.sh toolchain-subst-default
  assert_target tools/publish-toolchain-subst.sh toolchain-subst-default
  assert_target tests/td-subst.pub toolchain-subst-default
  assert_target tests/td-toolchain.lock toolchain-subst-default
  assert_target tests/td-toolchain.lock toolchain-input-addressed
  assert_target tests/td-toolchain.lock toolchain-x86_64-input-addressed
  assert_target tests/td-toolchain-x86_64.lock toolchain-x86_64-input-addressed
  assert_target tests/td-toolchain-x86_64.lock bootstrap-x86_64-toolchain-store-native
  assert_target tests/toolchain-x86_64-input-addressed.sh toolchain-x86_64-input-addressed
  assert_target mk/gates/418-toolchain-x86_64-input-addressed.mk toolchain-x86_64-input-addressed
  assert_target tests/ts/recipe-td-cmake-demo.ts cmake
  assert_target tests/td-cmake-demo.lock cmake
  assert_target tests/ts/recipe-uutils.ts rust-coreutils
  assert_target tests/uutils-coreutils.lock rust-coreutils
  assert_target tests/cat-uutils.lock rust-uutils
  assert_target tests/ts/recipe-youki.ts rust-youki
  assert_target tests/youki.lock rust-youki
  assert_target tests/cmake-demo/CMakeLists.txt cmake
  assert_target tests/ts/recipe-perturbed.ts drv-emit
  assert_target tests/guix-surface.sh guix-surface
  assert_target tests/guix-surface.expected guix-surface
  # The td-builder build engine validates on the check-engine SMOKE tier (Option B,
  # DESIGN §7.2): the full heavy+system corpus is the DAILY backstop, not a per-PR gate.
  assert_target builder/src/sandbox.rs check-engine
  assert_branch_policy builder/src/main.rs "full ./check.sh would be waived"
  assert_branch_policy builder/src/sandbox.rs "full ./check.sh would be waived"
  assert_branch_policy builder/Cargo.toml "full ./check.sh would be waived"
  assert_target system/td.scm check-system
  assert_branch_policy check.sh "full ./check.sh would be required"
  assert_branch_policy channels.scm "full ./check.sh would be required"
  assert_branch_policy system/td.scm "full ./check.sh would be required"
  assert_branch_policy DIGESTS.md "full ./check.sh would be required"
  assert_branch_policy new/unmapped.file "full ./check.sh would be required"

  if [ "$failures" -gt 0 ]; then
    echo "affected-checks self-test: $failures failure(s)" >&2
    return 1
  fi

  echo "PASS: affected-checks self-test"
}

if [ "$self_test" -eq 1 ]; then
  run_self_test
  exit $?
fi

if [ "${#explicit_paths[@]}" -gt 0 ]; then
  changed=$(printf '%s\n' "${explicit_paths[@]}" | sed '/^$/d' | LC_ALL=C sort -u)
else
  if ! git rev-parse --verify "$base^{commit}" >/dev/null 2>&1; then
    if [ "$base" = origin/main ] && git rev-parse --verify main^{commit} >/dev/null 2>&1; then
      base=main
    else
      echo "affected-checks: base ref '$base' is not available" >&2
      exit 2
    fi
  fi

  merge_base=$(git merge-base "$base" HEAD)
  changed=$(
    {
      git diff --name-only "$merge_base" HEAD
      if [ "$committed_only" -eq 0 ]; then
        git diff --name-only
        git diff --cached --name-only
        git ls-files --others --exclude-standard
      fi
    } | sed '/^$/d' | LC_ALL=C sort -u
  )
fi

if [ -z "$changed" ]; then
  echo "affected-checks: no changed paths relative to $base"
  exit 0
fi

while IFS= read -r p; do
  [ -n "$p" ] || continue
  map_path "$p"
done <<EOF
$changed
EOF

if [ "${#explicit_paths[@]}" -gt 0 ]; then
  echo "affected-checks: explicit path mode"
else
  echo "affected-checks: base=$base merge-base=$merge_base"
fi
echo
echo "Changed paths:"
while IFS= read -r p; do
  [ -n "$p" ] || continue
  echo "  $p"
done <<EOF
$changed
EOF
echo

if [ "${#preflights[@]}" -eq 0 ] && [ "${#targets[@]}" -eq 0 ]; then
  echo "Selected checks: none (docs-only or ignored local metadata)"
else
  echo "Selected checks:"
  for p in "${preflights[@]}"; do
    case "$p" in
      shell-syntax)       echo "  bash -n check.sh tests/*.sh ci/*.sh tools/*.sh .github/setup-branch-protection.sh" ;;
      cargo-test)         echo "  cargo test --manifest-path builder/Cargo.toml" ;;
      plan-index)         echo "  tools/plan-index.sh --check" ;;
      affected-self-test) echo "  tools/affected-checks.sh --self-test" ;;
    esac
  done
  if [ "${#targets[@]}" -gt 0 ]; then
    echo "  ./check.sh ${targets[*]}"
  fi
fi

echo
if [ "${#explicit_paths[@]}" -gt 0 ]; then
  echo "Waiver: inspection only (--path does not prove the branch diff)"
  if [ "${#full_required[@]}" -eq 0 ]; then
    echo "Branch-mode policy for these paths: full ./check.sh would be waived"
  else
    echo "Branch-mode policy for these paths: full ./check.sh would be required"
    for n in "${full_required[@]}"; do
      echo "  - $n"
    done
  fi
elif [ "${#full_required[@]}" -eq 0 ]; then
  echo "Waiver: full ./check.sh waived by affected-checks for this diff"
else
  echo "Waiver: full ./check.sh required before marking ready"
  for n in "${full_required[@]}"; do
    echo "  - $n"
  done
fi

if [ "${#notes[@]}" -gt 0 ]; then
  echo
  echo "Notes:"
  for n in "${notes[@]}"; do
    echo "  - $n"
  done
fi

if [ "$run" -eq 0 ]; then
  echo
  echo "Dry run only. Re-run with --run to execute."
  exit 0
fi

for p in "${preflights[@]}"; do
  case "$p" in
    shell-syntax)
      bash -n check.sh tests/*.sh ci/*.sh tools/*.sh .github/setup-branch-protection.sh ;;
    cargo-test)
      cargo test --manifest-path builder/Cargo.toml ;;
    plan-index)
      tools/plan-index.sh --check ;;
    affected-self-test)
      tools/affected-checks.sh --self-test ;;
  esac
done

if [ "${#full_required[@]}" -gt 0 ]; then
  if [ "${#explicit_paths[@]}" -gt 0 ]; then
    echo
    echo "affected-checks: --path is inspection only; run full ./check.sh for these paths in branch mode" >&2
    exit 20
  fi

  uncovered_targets=()
  skipped_targets=()
  for target in "${targets[@]}"; do
    if default_check_covers_target "$target"; then
      skipped_targets+=("$target")
    else
      uncovered_targets+=("$target")
    fi
  done

  if [ "${#uncovered_targets[@]}" -gt 0 ]; then
    ./check.sh "${uncovered_targets[@]}"
  fi
  if [ "${#skipped_targets[@]}" -gt 0 ]; then
    echo
    echo "affected-checks: escalation active; full ./check.sh covers skipped target(s): ${skipped_targets[*]}"
  fi

  echo
  echo "affected-checks: escalation active; running full ./check.sh"
  ./check.sh
elif [ "${#targets[@]}" -gt 0 ]; then
  ./check.sh "${targets[@]}"
fi
