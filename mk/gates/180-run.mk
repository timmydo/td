# 7. M8 run gate — execute the SHIPPED OCI image as a real rootless OCI container
#    (crun) and assert its userspace runs. Every gate above proves a PROPERTY of
#    the artifact (reproducible, guix-free, manifest-driven) but none ever RAN it;
#    this closes that gap. crun is the low-level OCI runtime podman drives (podman
#    itself is a ~1238-derivation Go tree with cold fetches — it breaks the offline
#    loop; crun is 18 derivations, offline). NOT a derivation: running a container
#    needs a live user namespace, which the build daemon's sandbox forbids, so —
#    exactly like `docker run` — this runs in the loop shell against the freshly
#    built image (check.sh exposes the host cgroup2 so crun's startup probe passes;
#    the helper runs crun rootless, --cgroup-manager=disabled, single-uid map,
#    empty network ns → the container is offline by construction). The image
#    entrypoint is the system boot-program (the full boot is covered by the
#    marionette `test`/`boot-disk` gates); here we OVERRIDE args like
#    `docker run IMG <cmd>` to drive /bin/sh. Self-discriminating: a positive run
#    (sentinel + exit 0) AND a negative control (a bogus exec must fail) — see
#    tests/run-image.sh. Heaviest behavioral gate (it unpacks the full image
#    rootfs) → runs last (§1.3). Its scratch (archive + unpacked rootfs) lives
#    in $(CURDIR)/.run-scratch — disk, not the sandbox /tmp (same tmpfs-size
#    lesson as .genimg-scratch above); run-image.sh's own EXIT trap cleans its
#    subdir either way, the recipe removes the parent on green.
HEAVY_GATES += run
run:
	@echo ">> run: execute the shipped OCI image as a real OCI container (crun)"
	@set -euo pipefail; \
	img=`$(GUIX) system image $(LOAD) -t docker $(SYSTEM)`; \
	test -n "$$img" || { echo "ERROR: could not build the shipped OCI image" >&2; exit 1; }; \
	echo ">> shipped OCI image: $$img"; \
	scratch="$(CURDIR)/.run-scratch"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	TMPDIR="$$scratch" sh tests/run-image.sh "$$img"; \
	rm -rf "$$scratch"
