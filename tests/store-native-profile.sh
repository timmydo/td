#!/bin/sh
# tests/store-native-profile.sh — prove `td-builder profile --store-native` builds a profile
# whose entries are LOGICAL /td/store symlinks that RESOLVE + RUN inside a store-ns own-root
# with /gnu/store ABSENT — the .scm-free userspace ASSEMBLY mechanism (no guix operating-system).
#
# The tool here is bash-static (from hello's PINNED closure, read by td's own store-closure
# reader — no guix process), the same cheap static runner store-ns.sh uses; it gives a real
# multi-entry package (bash + sh). This gate proves the ASSEMBLY + own-root execution; the
# guix-FREE /td/store-NATIVE userland the toolchain builds (bootstrap-hello-userland #192 /
# gcc-14 #197) joins this SAME mechanism. The
# profile --store-native logical-vs-physical link behaviour is unit-tested in builder/src.
#
# Legs:
#   [structural] profile entries are LOGICAL /td/store symlinks (dangle on the host, resolve
#                only in the own-root where /td/store is the bound store).
#   [behavioral] the profiled tools resolve via /td/store/profile/bin and RUN in the own-root.
#   [structural] /gnu/store is ABSENT in the own-root (unmixed from the guix install).
set -eu
fail() { echo "FAIL: $*" >&2; exit 1; }

. tests/cache-lib.sh
load_stage0 || fail "stage0-builder could not place a guix-free stage0 td-builder"
echo ">> td-builder (stage0, guix-free): $TB"

work=`mktemp -d`
trap 'chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"' EXIT INT TERM

# A static package from hello's PINNED closure (td's own store-closure reader, no guix process).
bash=`grep -- '-bash-' tests/hello-no-guix.lock | grep -v static | sed 's/^[^ ]* //' | head -1`
test -n "$bash" || fail "no bash in hello's lock"
bs=`"$TB" store-closure-scan /gnu/store "$bash" | grep -- '-bash-static-' | head -1`
test -n "$bs" -a -x "$bs/bin/bash" || fail "no static bash in the closure of $bash"

# Intern it at the LOGICAL /td/store (TD_STORE_DIR); bytes land physically under $store.
store="$work/td-store"; mkdir -p "$store"; db="$work/db.sqlite"
export TD_STORE_DIR=/td/store
pkg=`"$TB" store-add-recursive bash-static "$bs" "$store" "$db"` || fail "store-add-recursive bash-static"
case "$pkg" in /td/store/*-bash-static) ;; *) fail "bash-static not content-addressed at /td/store: $pkg" ;; esac
physpkg="$store/`basename "$pkg"`"
test -x "$physpkg/bin/bash" || fail "interned bash-static missing physically at $physpkg"

# Build a STORE-NATIVE profile: the links target the LOGICAL /td/store path (resolve in the
# own-root), enumerated from the physical package dir.
prof="$store/profile"
"$TB" profile --store-native "$prof" "$physpkg" || fail "profile --store-native"

# --- [structural] the profile entries are LOGICAL /td/store symlinks -------------------------
for t in bash sh; do
  tgt=`readlink "$prof/bin/$t"` || fail "no profile entry for $t"
  case "$tgt" in
    /td/store/*-bash-static/bin/"$t") ;;
    *) fail "profile/bin/$t is not a logical /td/store link (got: $tgt)" ;;
  esac
done
echo "   [structural] profile entries (bash, sh) are logical /td/store symlinks"

# --- run the profiled tools in the store-ns own-root (/td/store = $store, /gnu/store absent) --
# The probe is a FILE in the store (bound at /td/store/probe.sh in the own-root), so there is
# no nested-quoting between the outer command substitution and the inner script.
cat > "$store/probe.sh" <<'PROBE'
export PATH=/td/store/profile/bin
[ -e /gnu/store ] && echo GNU-PRESENT || echo GNU-ABSENT
case "$(command -v bash)" in /td/store/profile/bin/bash) echo BASH-VIA-PROFILE ;; esac
case "$(command -v sh)" in /td/store/profile/bin/sh) echo SH-VIA-PROFILE ;; esac
bash -c 'echo "BASH-RAN:$BASH_VERSION"'
sh -c 'echo SH-RAN-OK'
PROBE
out=$("$TB" store-ns "$store" -- "/td/store/profile/bin/bash" /td/store/probe.sh) \
  || { printf '%s\n' "$out" | sed 's/^/     /' >&2; fail "store-ns profile run exited nonzero"; }
printf '%s\n' "$out" | sed 's/^/     /'

# --- [behavioral] + [structural] -------------------------------------------------------------
printf '%s\n' "$out" | grep -q '^BASH-VIA-PROFILE$' || fail "bash did not resolve via /td/store/profile/bin"
printf '%s\n' "$out" | grep -q '^SH-VIA-PROFILE$' || fail "sh did not resolve via /td/store/profile/bin"
printf '%s\n' "$out" | grep -q '^BASH-RAN:5' || fail "the profiled bash did not run from /td/store"
printf '%s\n' "$out" | grep -q '^SH-RAN-OK$' || fail "the profiled sh did not run from /td/store"
echo "   [behavioral] the profiled tools resolve via /td/store/profile/bin and RUN from /td/store"
printf '%s\n' "$out" | grep -q '^GNU-ABSENT$' || fail "/gnu/store is PRESENT in the own-root — mixed with the guix install"
echo "   [structural] /gnu/store is ABSENT in the own-root (unmixed from the guix install)"

echo "PASS: store-native-profile — td-builder profile --store-native builds a profile of LOGICAL"
echo "  /td/store links that resolve + RUN in the store-ns own-root, /gnu/store ABSENT. The"
echo "  .scm-free userspace assembly mechanism the /td/store-native userland slots into."
