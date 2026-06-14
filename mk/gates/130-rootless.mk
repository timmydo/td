# 10. rootless-builder differential (DESIGN §7.1 side-track; prime directive 4).
#    Build the target with a ROOTLESS USER-NAMESPACE builder and prove
#    daemon-vs-rootless store-path equality — the root guix-daemon is the
#    oracle. The rootless builder is the SAME pinned daemon binary run
#    UNPRIVILEGED in a nested userns (at this pin a daemon without
#    --build-users-group gives every chroot build CLONE_NEWUSER), so privilege +
#    namespace is the ONLY variable in the experiment. tests/rootless.sh:
#      • stages a writable /gnu/store view (per-item binds + rbind; overlayfs
#        is impossible here — the sandbox's per-item profile binds are
#        MNT_LOCKED in the nested userns and overlay rejects such a lowerdir),
#        snapshots the host DB via sqlite's backup API, covers /var/guix with
#        tmpfs (the host daemon is unreachable by construction), and starts the
#        unprivileged daemon offline (--no-substitutes --no-offload);
#      • validity guard: the oracle output must be valid in the snapshot —
#        otherwise `--check` would BUILD instead of COMPARE (false green);
#      • isolation probe: a deliberately environment-sensitive drv records
#        /proc/self/uid_map from inside the build; an identity map (no userns)
#        reds the gate. The probe is an instrument, never `--check`ed, and its
#        output exists only in the discarded scratch store (it must stay
#        INVALID in the real store — the guard reds if it ever becomes valid);
#      • the differential: rootless `guix build --check` of the SAME image drv
#        the `build` gate oracles — same drv ⇒ same store path by construction
#        (asserted explicitly), and --check makes the rootless daemon rebuild
#        it and compare bit-for-bit against the root daemon's artifact. On
#        mismatch the divergent rebuild is kept (--keep-failed) and the gate
#        prints the exact diffoscope command to run OUTSIDE the loop
#        (diffoscope is a cold Python closure the offline sandbox cannot
#        build).
#    The recipe does the pinned-guix work ($(GUIX): lower, oracle-build,
#    closure via gc -R); the script does the namespace work with the
#    pin-guarded host guix (time-machine cannot re-resolve channels once
#    /gnu/store is covered). Scratch lives in $(CURDIR)/.rootless-scratch
#    (disk, not the sandbox tmpfs); kept on red for diffing, removed on green.
HEAVY_GATES += rootless
rootless:
	@echo ">> rootless: unprivileged userns builder vs root daemon — store-path differential"
	@set -euo pipefail; \
	test -n "$${GUIX_ENVIRONMENT-}" || { echo "ERROR: GUIX_ENVIRONMENT is unset — run via ./check.sh (the sandbox profile must be bound into the staged store)" >&2; exit 1; }; \
	img_drv=`$(GUIX) system image $(LOAD) -t $(IMGTYPE) -d $(SYSTEM)`; \
	test -n "$$img_drv" || { echo "ERROR: could not lower the image derivation" >&2; exit 1; }; \
	echo ">> target image drv: $$img_drv"; \
	echo ">> oracle build via the ROOT daemon"; \
	img_out=`$(GUIX) build "$$img_drv"`; \
	test -n "$$img_out" || { echo "ERROR: oracle build produced no output path" >&2; exit 1; }; \
	scratch="$(CURDIR)/.rootless-scratch"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	TD_IMAGE_DRV="$$img_drv" $(GUIX) repl $(LOAD) tests/rootless-drvs.scm 2>/dev/null > "$$scratch/drvs.txt"; \
	img_out_lowered=`sed -n 's/^IMG_OUT=//p' "$$scratch/drvs.txt" | head -n1`; \
	probe_drv=`sed -n 's/^PROBE_DRV=//p' "$$scratch/drvs.txt"`; \
	probe_out=`sed -n 's/^PROBE_OUT=//p' "$$scratch/drvs.txt"`; \
	test -n "$$img_out_lowered" -a -n "$$probe_drv" -a -n "$$probe_out" || { echo "ERROR: could not lower the rootless rung derivations" >&2; exit 1; }; \
	test "$$img_out_lowered" = "$$img_out" || { echo "ERROR: lowered image output ($$img_out_lowered) != realized oracle output ($$img_out)" >&2; exit 1; }; \
	guix_pkg=`dirname "$$(dirname "$$(readlink -f "$$(command -v guix)")")"`; \
	guix_daemon_pkg=`dirname "$$(dirname "$$(readlink -f "$$(command -v guix-daemon)")")"`; \
	{ sed -n 's/^IMG_INPUT=//p;s/^PROBE_INPUT=//p' "$$scratch/drvs.txt"; \
	  printf '%s\n' "$$img_drv" "$$img_out" "$$probe_drv" "$$guix_pkg" "$$guix_daemon_pkg" "$$GUIX_ENVIRONMENT"; } \
	  | xargs $(GUIX) gc -R | sort -u > "$$scratch/paths.txt"; \
	echo ">> bind closure: $$(wc -l < "$$scratch/paths.txt") store items"; \
	bash tests/rootless.sh "$$scratch" "$$img_drv" "$$img_out" "$$probe_drv" "$$probe_out"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"
