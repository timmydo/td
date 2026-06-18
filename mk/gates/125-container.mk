# 8. M9.2 container-HOST gate — boot the SHIPPED base and run a Guix-built OCI APP
#    image on it with the shipped crun, as root. Where `run` (M8) ran the shipped
#    SYSTEM image's userspace, this runs a SEPARATE app image ON the booted base —
#    the container-host relationship (DESIGN §2.3 OCI app model). The app is
#    `guix pack -f docker` of GNU hello (a store path → offline, no registry); it
#    is unpacked into a runtime-bundle rootfs at build time, then crun runs it AS
#    ROOT in the guest (no rootless/userns contortions — that was M8's sandbox-only
#    concern; M9.1 made the base a host: cgroup2 mounted + crun shipped). Marionette
#    gate, so it lowers-then-realises like `test`/`boot-disk` for an honest exit
#    status. The app runs via the IMAGE'S OWN declared entrypoint (read from its
#    archive — a bogus #:entry-point fails the positive, F1). First `--check`s ALL
#    FOUR app artifacts — the good image+bundle AND the bad-entrypoint image+bundle
#    used by the image-metadata negative — so every artifact is permanently proven
#    reproducible (CLAUDE.md), not just the good one. Then runs. Self-discriminating:
#    a POSITIVE run (app prints "Hello, world!", exit 0) and TWO negative controls (a
#    second image with a bogus DECLARED entrypoint, and a bogus runtime arg, both must
#    fail) — see tests/container.scm. M9.3 ADDS a managed-cgroups assertion: crun
#    (cgroupfs manager) applies a declared pids.max=73 to a coreutils container, which
#    reads its own /sys/fs/cgroup/pids.max back as 73 — resource-limit ENFORCEMENT, not
#    just that crun starts (self-discriminating: the cgroup2 default is "max"). The
#    cgroup app image+bundle are --checked for reproducibility alongside the others.
#    fhs-app-images ADDS an FHS-LAYOUT app image+bundle (hello with a /usr/bin/hello
#    symlink, also --checked): crun execs the explicit /usr/bin/hello against the FHS
#    rootfs (resolves, prints output) while the SAME arg fails on the plain
#    store-layout rootfs — proving the binary resolves at a traditional FHS path.
SYSTEM_GATES += container
container:
	@echo ">> container: run an OCI app container on the booted td base (crun)"
	@set -euo pipefail; \
	arts=`printf '%s\n' \
	    '(use-modules (guix) (guix monads) (tests container))' \
	    '(with-store store' \
	    '  (set-build-options store #:use-substitutes? #f #:offload? #f)' \
	    '  (let ((img  (run-with-store store (td-app-image)))' \
	    '        (bun  (run-with-store store (td-app-bundle)))' \
	    '        (bimg (run-with-store store (td-app-badentry-image)))' \
	    '        (bbun (run-with-store store (td-app-badentry-bundle)))' \
	    '        (cimg (run-with-store store (td-app-cgroup-image)))' \
	    '        (cbun (run-with-store store (td-app-cgroup-bundle)))' \
	    '        (fimg (run-with-store store (td-app-fhs-image)))' \
	    '        (fbun (run-with-store store (td-app-fhs-bundle))))' \
	    '    (format #t "IMAGE=~a~%" (derivation-file-name img))' \
	    '    (format #t "BUNDLE=~a~%" (derivation-file-name bun))' \
	    '    (format #t "BADIMAGE=~a~%" (derivation-file-name bimg))' \
	    '    (format #t "BADBUNDLE=~a~%" (derivation-file-name bbun))' \
	    '    (format #t "CGIMAGE=~a~%" (derivation-file-name cimg))' \
	    '    (format #t "CGBUNDLE=~a~%" (derivation-file-name cbun))' \
	    '    (format #t "FHSIMAGE=~a~%" (derivation-file-name fimg))' \
	    '    (format #t "FHSBUNDLE=~a~%" (derivation-file-name fbun))))' \
	  | $(GUIX) repl $(LOAD) 2>/dev/null`; \
	img=`printf '%s\n' "$$arts" | sed -n 's/^IMAGE=//p'`; \
	bun=`printf '%s\n' "$$arts" | sed -n 's/^BUNDLE=//p'`; \
	bimg=`printf '%s\n' "$$arts" | sed -n 's/^BADIMAGE=//p'`; \
	bbun=`printf '%s\n' "$$arts" | sed -n 's/^BADBUNDLE=//p'`; \
	cimg=`printf '%s\n' "$$arts" | sed -n 's/^CGIMAGE=//p'`; \
	cbun=`printf '%s\n' "$$arts" | sed -n 's/^CGBUNDLE=//p'`; \
	fimg=`printf '%s\n' "$$arts" | sed -n 's/^FHSIMAGE=//p'`; \
	fbun=`printf '%s\n' "$$arts" | sed -n 's/^FHSBUNDLE=//p'`; \
	test -n "$$img" -a -n "$$bun" -a -n "$$bimg" -a -n "$$bbun" -a -n "$$cimg" -a -n "$$cbun" -a -n "$$fimg" -a -n "$$fbun" || { echo "ERROR: could not lower the app artifacts" >&2; exit 1; }; \
	echo ">> app artifacts: image=$$img bundle=$$bun"; \
	echo ">> negative-control artifacts: badimage=$$bimg badbundle=$$bbun"; \
	echo ">> cgroup artifacts (M9.3): cgimage=$$cimg cgbundle=$$cbun"; \
	echo ">> fhs artifacts (fhs-app-images): fhsimage=$$fimg fhsbundle=$$fbun"; \
	$(GUIX) build "$$img" "$$bun" "$$bimg" "$$bbun" "$$cimg" "$$cbun" "$$fimg" "$$fbun" >/dev/null; \
	echo ">> reproducibility: guix build --check the app images + extracted bundles (good + negative + cgroup + fhs; verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$img" "$$bun" "$$bimg" "$$bbun" "$$cimg" "$$cbun" "$$fimg" "$$fbun"; \
	drv=`printf '%s\n' \
	    '(use-modules (guix) (gnu tests) (tests container))' \
	    '(with-store store' \
	    '  (set-build-options store #:use-substitutes? #f #:offload? #f)' \
	    '  (format #t "DRV=~a~%"' \
	    '          (derivation-file-name' \
	    '           (run-with-store store (system-test-value %test-td-container)))))' \
	  | $(GUIX) repl $(LOAD) 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the container test derivation" >&2; exit 1; }; \
	echo ">> realise container test derivation: $$drv"; \
	$(GUIX) build "$$drv"
