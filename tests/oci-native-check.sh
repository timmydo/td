#!/bin/sh
# tests/oci-native-check.sh — behavioral check for a td-NATIVE OCI image (the
# system-image-native track: td-builder builds the .drv-free image, no
# `guix system image`). Given a docker-archive built by `td-builder oci-image-closure`,
# prove it is a real, working OCI image with FOREIGN tools — the durable replacement for
# the retired guix-system-image gates:
#
#   1. skopeo `copy docker-archive:` loads it into an OCI layout and yields a sha256
#      manifest digest (a foreign OCI implementation accepts td's bytes);
#   2. crun RUNS it (rootless, no host store bound — the image is self-contained) and
#      the named ENTRYPOINT binary emits the EXPECTED output;
#   3. a bogus exec in the same image FAILS (self-discrimination — a green that can tell
#      a working image from a broken one).
#
# The td image is an UNCOMPRESSED docker-archive (td-builder is zero-dep), so `tar xf`
# (auto-detect) unpacks both it and guix's gzipped archives.
#
# Usage: oci-native-check.sh IMAGE.tar ENTRYPOINT-ABS-PATH EXPECTED-SUBSTRING [ARG...]
#   env: SKOPEO=<skopeo bin> CRUN=<crun bin>
#   ARGs (optional) are appended to the entrypoint (e.g. `… crun 'crun version' --version`).
set -eu

IMG=${1:?usage: oci-native-check.sh IMAGE ENTRYPOINT EXPECTED [ARG...]}
ENTRY=${2:?missing in-image entrypoint path}
EXPECT=${3:?missing expected output substring}
SKOPEO=${SKOPEO:?set SKOPEO to the skopeo binary}
CRUN=${CRUN:?set CRUN to the crun binary}
# Any args after EXPECTED are passed to the entrypoint (e.g. `crun --version`),
# so a self-contained binary that needs a flag to emit output still runs cleanly.
shift 3
args_json="[ \"$ENTRY\""
for a in "$@"; do args_json="$args_json, \"$a\""; done
args_json="$args_json ]"

WORK=$(mktemp -d)
cleanup() { chmod -R u+w "$WORK" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT
cd "$WORK"

# --- (1) foreign-tool load: skopeo parses the archive + re-derives a manifest digest.
echo ">> skopeo copy docker-archive -> oci layout"
"$SKOPEO" --tmpdir "$WORK" copy --insecure-policy "docker-archive:$IMG" "oci:$WORK/layout:td" >/dev/null
digest=$("$SKOPEO" --tmpdir "$WORK" inspect --format '{{.Digest}}' "oci:$WORK/layout:td")
case "$digest" in
  sha256:*) echo "   ok: skopeo loaded the td-native image (manifest $digest)" ;;
  *) echo "FAIL: skopeo produced no sha256 manifest digest (got: '$digest')" >&2; exit 1 ;;
esac

# --- unpack the single layer into an OCI runtime bundle rootfs (no host store bound:
# a clean run proves the image carries its own closure).
mkdir -p extract rootfs
tar xf "$IMG" -C extract
layer=$(find extract -name layer.tar | head -1)
test -n "$layer" || { echo "FAIL: no layer.tar inside $IMG" >&2; exit 1; }
tar xf "$layer" -C rootfs
test -f "rootfs/${ENTRY#/}" || { echo "FAIL: entrypoint $ENTRY absent from the image rootfs" >&2; exit 1; }

HOSTUID=$(id -u)
HOSTGID=$(id -g)
gen_config() {  # $1 = JSON array literal for process.args
  cat > config.json <<EOF
{
  "ociVersion": "1.0.0",
  "process": {
    "terminal": false,
    "user": { "uid": 0, "gid": 0 },
    "args": $1,
    "env": [ "PATH=/bin:/usr/bin", "HOME=/", "TERM=dumb" ],
    "cwd": "/",
    "noNewPrivileges": true
  },
  "root": { "path": "rootfs", "readonly": true },
  "hostname": "td-oci-native",
  "mounts": [
    { "destination": "/proc", "type": "proc", "source": "proc" },
    { "destination": "/dev", "type": "tmpfs", "source": "tmpfs",
      "options": [ "nosuid", "strictatime", "mode=755", "size=65536k" ] }
  ],
  "linux": {
    "uidMappings": [ { "containerID": 0, "hostID": ${HOSTUID}, "size": 1 } ],
    "gidMappings": [ { "containerID": 0, "hostID": ${HOSTGID}, "size": 1 } ],
    "namespaces": [
      { "type": "pid" }, { "type": "ipc" }, { "type": "uts" },
      { "type": "mount" }, { "type": "user" }, { "type": "network" }
    ]
  }
}
EOF
}
run_ctr() { "$CRUN" --cgroup-manager=disabled --root="$WORK/state" run "$1"; }

# --- (2) POSITIVE: the entrypoint runs and emits EXPECTED.
echo ">> crun run: $ENTRY $*"
gen_config "$args_json"
if ! out=$(run_ctr td-oci-native-pos); then
  echo "FAIL: the td-native OCI image did not run (crun exited non-zero)" >&2
  printf '%s\n' "$out" >&2
  exit 1
fi
printf '%s\n' "$out"
printf '%s\n' "$out" | grep -qF "$EXPECT" \
  || { echo "FAIL: entrypoint ran but did not emit '$EXPECT'" >&2; exit 1; }
echo "   ok: the td-native image's userspace ran ('$EXPECT' received)"

# --- (3) NEGATIVE control: a non-existent binary MUST fail the run.
echo ">> negative: exec a non-existent binary must FAIL"
gen_config '[ "/gnu/store/td-nonexistent-xyz/bin/nope" ]'
if run_ctr td-oci-native-neg >/dev/null 2>&1; then
  echo "FAIL: running a non-existent binary SUCCEEDED — the check cannot discriminate." >&2
  exit 1
fi
echo "   ok: a non-running exec was correctly detected (the check discriminates)"

echo "PASS: a td-NATIVE OCI image (built by td-builder oci-image-closure, no guix system image) loads via skopeo and runs '$ENTRY' as a real rootless OCI container emitting '$EXPECT'; self-discriminating."
