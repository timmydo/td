#!/bin/sh
# tests/run-image.sh — M8 behavioral rung helper (DESIGN §2.4 step 5 / §6).
#
# Every prior rung proves a PROPERTY of the shipped artifact — it is reproducible
# (`oci`), guix-free by construction (`no-guix`), manifest-driven (`manifest-*`) —
# but none ever RAN it. This rung closes that gap: it executes the shipped OCI
# image as a real, rootless OCI container with crun (the low-level runtime podman
# itself drives) and asserts the image's userspace actually runs.
#
# Why crun, not podman: podman pulls a ~1238-derivation Go tree with cold source
# fetches — it breaks the offline/warm-loop contract. crun is 18 derivations,
# offline-buildable, and is the component podman delegates the run to. Why not in
# a derivation: running a container needs a live user namespace, which the build
# daemon's own sandbox forbids; so, exactly like `docker run`, this executes in
# the loop shell against the freshly built image. Feasibility (nested rootless
# userns + crun's pivot_root/mount dance inside `guix shell -C`) was proven before
# this rung was written; check.sh exposes the host cgroup2 mount so crun's startup
# cgroup probe passes, and runs crun with --cgroup-manager=disabled (no cgroup
# delegation in the sandbox), a single-uid map (the sandbox grants exactly one
# uid), and an empty network namespace (the container is offline by construction).
#
# The image's configured entrypoint is the system boot-program (the full boot is
# exercised by the marionette `test`/`boot-disk` rungs). Here we OVERRIDE the
# process args — exactly as `docker run IMAGE <cmd>` does — to drive the container
# non-interactively. NOTE on paths: a guix system image's FHS conveniences
# (/bin/sh, /run/current-system) are materialised at BOOT by the entrypoint's
# activation; an unpacked, un-booted image has real executables only under
# /gnu/store/.../bin. So we exec a shell DISCOVERED at its store path in the
# image's own rootfs — a genuine ELF from the shipped artifact, run via its own
# glibc loader. (M9 dropped the static-FHS-on-base idea in favour of a minimal
# container-HOST base: it ships crun + mounts cgroup2 and runs apps in OCI
# containers — see tests/container.scm — rather than flattening app paths into the
# base itself.)
#
# Self-discriminating (the M3 lesson — a green rung is only meaningful once seen
# red): a POSITIVE run must emit a sentinel and exit 0, AND a NEGATIVE control
# (a bogus exec) must FAIL. Running the positive first proves container setup is
# sound, so the negative isolates "did the image's binary actually run".
set -eu

IMG="${1:?usage: run-image.sh IMAGE.tar.gz}"   # the built shipped docker image

command -v crun >/dev/null 2>&1 || { echo "FAIL: crun is not on PATH (add it to the sandbox toolchain)" >&2; exit 1; }

WORK=$(mktemp -d)
cleanup() { chmod -R u+w "$WORK" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

HOSTUID=$(id -u)
HOSTGID=$(id -g)

# Unpack the docker archive into an OCI runtime bundle rootfs. The image carries
# its OWN /gnu/store, so we bind NO host store — a clean run proves the artifact
# is self-contained.
cd "$WORK"
mkdir -p extract rootfs
tar xzf "$IMG" -C extract
layer=$(find extract -name layer.tar | head -1)
test -n "$layer" || { echo "FAIL: no layer.tar inside $IMG (corrupt or unexpected image format)" >&2; exit 1; }
tar xf "$layer" -C rootfs

# Discover a real shell ELF inside the image's own store. We target bin/bash (the
# real binary; bin/sh is a symlink to it). The in-container path is the rootfs
# path with the rootfs/ prefix stripped — i.e. its absolute path as the container
# sees it.
shrel=$(cd rootfs && find gnu/store -maxdepth 4 -type f -path '*/bin/bash' 2>/dev/null | head -1)
test -n "$shrel" || { echo "FAIL: no bash found in the image rootfs (cannot exercise the image's userspace)" >&2; exit 1; }
SH="/$shrel"
echo ">> shell discovered in the shipped image: $SH"

# Write an OCI runtime config.json that execs the given args. Rootless: single-uid
# map (containerID 0 -> our host uid, size 1), empty network ns, no cgroups.
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
  "hostname": "td-run",
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

run_ctr() {  # $1 = container id (config.json in cwd)
  crun --cgroup-manager=disabled --root="$WORK/state" run "$1"
}

# === POSITIVE: the shipped image's shell runs and emits the sentinel ===
echo ">> positive: run the shipped image's shell as an OCI container"
gen_config "[ \"$SH\", \"-c\", \"echo TD_RUN_OK; exit 0\" ]"
if ! out=$(run_ctr td-run-pos); then
  echo "FAIL: the shipped OCI image did not run (crun exited non-zero)" >&2
  printf '%s\n' "$out" >&2
  exit 1
fi
printf '%s\n' "$out"
printf '%s\n' "$out" | grep -q '^TD_RUN_OK$' \
  || { echo "FAIL: sentinel TD_RUN_OK was not emitted — the shell ran but produced the wrong output." >&2; exit 1; }
echo "   ok: the shipped OCI image's userspace ran (sentinel received, exit 0)"

# === NEGATIVE control (discriminator): a bogus exec MUST fail the run ===
echo ">> negative: exec a non-existent binary in the same image must FAIL"
gen_config '[ "/gnu/store/td-nonexistent-xyz/bin/sh" ]'
if run_ctr td-run-neg >/dev/null 2>&1; then
  echo "FAIL: running a non-existent binary SUCCEEDED — the rung cannot tell a running image from a broken one; its green is meaningless." >&2
  exit 1
fi
echo "   ok: a non-running exec was correctly detected as a failure (the rung discriminates)"

echo "PASS: the shipped OCI image runs as a real rootless OCI container (crun) — its userspace executes (positive) and the rung is self-discriminating (negative control fails)."
