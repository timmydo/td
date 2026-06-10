# td — the single pass/fail entry point (CLAUDE.md "The loop").
#
# `make check` runs, in order and short-circuiting on the first failure:
#   1. eval           — load declaration + test modules (fails fast, sub-second)
#   2. diff           — typed front-end lowers to the same SYSTEM drv as the gexp (M4)
#   3. typed-coverage — every typed field is wired into the system + validated (M4)
#   4. oci-diff       — typed front-end lowers to the same OCI image drv as the gexp (M5)
#   5. manifest-diff  — a changed manifest swaps to a different OCI image (M6)
#   6. build          — build the bootable image and assert it is reproducible
#   7. test           — boot the marionette system test and assert behaviors
#   8. boot-disk      — boot the qcow2 through GRUB (real bootloader path) + kernel
#   9. oci            — build the Docker/OCI image and assert it is reproducible (M5)
#  10. manifest-check — build a swapped-manifest image, --check it, and assert the
#                       declared package is actually in the realized tarball (M6)
#  11. no-guix        — build the hardened (ship-guix? #f) image, --check it, and
#                       assert the imperative guix/guix-daemon surface is absent
#                       from it but present in an explicit ship-guix? #t CONTROL
#                       image; plus a whole-system gate over the SHIPPED system
#                       (must build) and over a service-injection fixture (must
#                       fail at the gate) — review F1 (M7)
#  12. run            — execute the SHIPPED OCI image as a real rootless OCI
#                       container (crun) and assert its userspace runs: a positive
#                       run (sentinel + exit 0) and a negative control (a bogus
#                       exec must fail). Closes the gap that every prior rung
#                       proved a PROPERTY of the artifact but never RAN it (M8)
#  13. container      — boot the shipped base and run a Guix-built OCI APP image
#                       (guix pack -f docker hello) ON it with the shipped crun, as
#                       root: assert the app prints its output + exits 0, with a
#                       negative control. Proves td is a working container HOST (M9).
#                       M9.3 also asserts crun ENFORCES a declared pids.max=73 on a
#                       coreutils container (cgroupfs manager) — the container reads
#                       its own /sys/fs/cgroup/pids.max back as 73 (managed cgroups)
#
# Every guix invocation is pinned to channels.scm via `guix time-machine`, so
# the reproducibility oracle is honest regardless of the ambient guix version.
# Run it via `./check.sh` (the hermetic, offline wrapper) — NOT a bare
# `guix shell -C --pure -- make check`, which lacks the store/daemon exposure,
# host-guix-pin guard, and substitute-disabling that keep the loop offline.

# Recipes use bash so multi-command recipes can run under `set -euo pipefail`
# (triage #1): a failure ANYWHERE in a `;`-chained recipe — notably a
# `guix build --check` reproducibility failure or an unreadable artifact — must
# abort the rung, never be swallowed so a later command's success greens it.
SHELL   := bash

GUIX    := guix time-machine -C channels.scm --
LOAD    := -L .
SYSTEM  := system/td.scm
IMGTYPE := qcow2

# Bare `make` runs the in-sandbox loop, never the sandbox wrapper — guards
# against `container-check` (which calls ./check.sh) being the default goal and
# recursing into nested containers.
.DEFAULT_GOAL := check

.PHONY: check container-check eval diff typed-coverage oci-diff manifest-diff generation-diff build test boot-disk oci manifest-check generation-image no-guix run container

# The hermetic, offline, self-contained entry point (DESIGN §1.1/§1.4). Plain
# `make check` assumes you are ALREADY inside the right `guix shell -C` sandbox;
# `make container-check` (or ./check.sh) sets that sandbox up for you. Prefer it.
container-check:
	@./check.sh

check: eval diff typed-coverage oci-diff manifest-diff generation-diff build test boot-disk oci manifest-check generation-image no-guix run container

# 1. Config eval — load every module; catches syntax/binding errors in well
#    under a second, before any expensive build. Run as a repl SCRIPT, NOT piped
#    via STDIN: `guix repl` reading from STDIN always exits 0 (swallows the
#    script's status), which made a broken module pass `eval` green. `guix repl
#    FILE` honors the exit code, so a load error reddens this rung honestly.
eval:
	@echo ">> eval: load (system td), (system td-typed), (tests boot) and (tests container)"
	$(GUIX) repl $(LOAD) tests/eval.scm

# M4 differential (DESIGN §2.4/§2.5). Cheap structural check — lowers systems to
# derivations, no building — so it runs right after eval and fails fast. Run as
# a repl SCRIPT (not piped via STDIN) so the script's `(exit)` is the rung's
# exit status; a piped script would always exit 0 and hide a red (see `test`).
diff:
	@echo ">> diff: typed front-end lowers to the same store path as the gexp"
	$(GUIX) repl $(LOAD) tests/typed-diff.scm

# M4 typed coverage (triage #4). Table-driven, derivation-level: every typed
# field must (A) change the lowered system when given a valid non-default value
# (proves it is wired, not ignored) and (B) reject an invalid value at
# construction (proves per-field validation). Where `diff` checks convergence +
# one perturbation, this sweeps all fields. Run as a repl SCRIPT for honest exit.
typed-coverage:
	@echo ">> typed-coverage: every typed field is wired and validated"
	$(GUIX) repl $(LOAD) tests/typed-coverage.scm

# M5 OCI differential (DESIGN §2.4 step 5/§2.5). Same cheap, derivation-level,
# self-discriminating shape as `diff`, but the artifact is the Docker/OCI image
# derivation: prove the typed front-end drives the OCI image too, and that a
# changed config diverges. No image is built here — the bit-for-bit repro check
# is the `oci` rung below. Run as a repl SCRIPT so `(exit)` is the rung's status.
oci-diff:
	@echo ">> oci-diff: typed front-end lowers to the same OCI image drv as the gexp"
	$(GUIX) repl $(LOAD) tests/oci-diff.scm

# M6 manifest-swap differential (DESIGN §6: manifest-driven, image-swap-only).
# Cheap, derivation-level, self-discriminating like `oci-diff`, but the lever is
# the typed config's `manifest` field: (a) the default manifest converges to the
# frozen OCI oracle; (b) a manifest that adds one package (hello) lowers to a
# DIFFERENT OCI image — a wholesale image swap; (c) the added package is in the
# swapped system's package set and absent from the default's. No image is built
# here — the bit-for-bit repro of a SWAPPED generation is the `manifest-check`
# rung below. Run as a repl SCRIPT so `(exit)` is the rung's status.
manifest-diff:
	@echo ">> manifest-diff: a changed manifest swaps the whole OCI image"
	$(GUIX) repl $(LOAD) tests/manifest-diff.scm

# M10.1 per-generation root (DESIGN §2.3 generations; M10-design.md P1). Cheap,
# derivation/record-level, self-discriminating like the diffs above: prove the
# typed `generation` field derives a DISTINCT, bootloader-selectable root per
# generation — (a) generation #f still converges to the shared-root oracle, (b)
# two generations get different root labels AND different system drvs, (c) a
# generation's root is not the shared td-root. Without this each generation would
# boot the same filesystem and rollback would be a no-op. The full boot+rollback
# is M10.3. Run as a repl SCRIPT so `(exit)` is the rung's status.
generation-diff:
	@echo ">> generation-diff: each generation gets a distinct, selectable root (M10.1)"
	$(GUIX) repl $(LOAD) tests/generation-diff.scm

# 2. Reproducibility oracle — build the image, then rebuild its derivation with
#    --check (bit-for-bit identical or it is a FAILING test).
build:
	@echo ">> build: $(SYSTEM) image ($(IMGTYPE))"
	$(GUIX) system image $(LOAD) -t $(IMGTYPE) $(SYSTEM)
	@echo ">> check: reproducibility of the image derivation"
	$(GUIX) build --check \
	  $$($(GUIX) system image $(LOAD) -t $(IMGTYPE) -d $(SYSTEM))

# 3. Boot + behavioral — realise the marionette test derivation. Its builder
#    runs the SRFI-64 assertions in/against a booted VM and exits non-zero if any
#    fail, so a failed assertion makes this rung go red (see the two-step note in
#    the recipe for why we must NOT pipe the build into `guix repl`).
test:
	@echo ">> test: boot marionette + assert behaviors"
	@# Two steps on purpose. `guix repl` reading a script from STDIN always
	@# exits 0 (it swallows the script's exit code), so building the test there
	@# would make a FAILED test look green. Instead: (1) lower the monadic test
	@# value to a derivation file name via repl, then (2) realise it with
	@# `guix build`, whose exit status is honest and which streams the marionette
	@# log so failures are visible.
	@drv=`printf '%s\n' \
	    '(use-modules (guix) (gnu tests) (tests boot))' \
	    '(with-store store' \
	    '  (set-build-options store #:use-substitutes? #f #:offload? #f)' \
	    '  (format #t "DRV=~a~%"' \
	    '          (derivation-file-name' \
	    '           (run-with-store store (system-test-value %test-td-boot)))))' \
	  | $(GUIX) repl $(LOAD) 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the test derivation" >&2; exit 1; }; \
	echo ">> realise test derivation: $$drv"; \
	$(GUIX) build "$$drv"

# 3b. Disk-image boot (triage #2) — boot the qcow2 through its GRUB bootloader
#     (not the direct-kernel VM the `test` rung uses), so the bootloader,
#     partition table and disk image are actually exercised. Same honest two-step
#     lower-then-realise as `test`. Heavier (builds a second full image + boots
#     it), so it runs after the cheap rungs.
boot-disk:
	@echo ">> boot-disk: boot the qcow2 disk through GRUB + assert kernel"
	@drv=`printf '%s\n' \
	    '(use-modules (guix) (gnu tests) (tests boot))' \
	    '(with-store store' \
	    '  (set-build-options store #:use-substitutes? #f #:offload? #f)' \
	    '  (format #t "DRV=~a~%"' \
	    '          (derivation-file-name' \
	    '           (run-with-store store (system-test-value %test-td-disk-boot)))))' \
	  | $(GUIX) repl $(LOAD) 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the disk-boot test derivation" >&2; exit 1; }; \
	echo ">> realise disk-boot test derivation: $$drv"; \
	$(GUIX) build "$$drv"

# 4. OCI reproducibility oracle (M5) — same shape as `build`, but for the
#    Docker/OCI image: build it, then rebuild its derivation with --check
#    (bit-for-bit identical or it is a FAILING test, prime directive 1). The
#    OS closure is shared with `build`, so --check mostly re-runs the cheap
#    docker-packing step. The matching declaration also boots as a VM (M1–M4),
#    closing the north-star "one declaration, store-based + OCI" loop (DESIGN §0).
oci:
	@echo ">> oci: $(SYSTEM) image (docker)"
	$(GUIX) system image $(LOAD) -t docker $(SYSTEM)
	@echo ">> check: reproducibility of the OCI image derivation"
	$(GUIX) build --check \
	  $$($(GUIX) system image $(LOAD) -t docker -d $(SYSTEM))

# 5. M6 manifest-swap reproducibility — build a SWAPPED-manifest OCI image
#    generation (default manifest + GNU hello) and `--check` it bit-for-bit.
#    `manifest-diff` proves a changed manifest is a DIFFERENT image; this proves
#    that swapped generation is itself reproducible (DESIGN §6 image-swap-only;
#    prime directive 1 — a non-reproducible swapped image is a FAILING test).
#    The less-frequent/heavier rung (§1.3): it repacks a second docker tarball,
#    but hello's closure is tiny and the OS closure is shared with `oci`, so it
#    stays in budget. Two-step (lower to a drv via repl, then realise+check via
#    `guix build`) for the same honest-exit-status reason as the `test` rung.
#    It ALSO inspects the realized tarball (triage #5): manifest-diff only proves
#    the package is in the declaration (operating-system-packages) — an exporter
#    bug could change the image derivation yet omit the files. So here we crack
#    open the built layer.tar and assert hello/bin/hello is actually present in
#    the SWAPPED image and ABSENT from the default image — artifact contents, not
#    just the declaration.
manifest-check:
	@echo ">> manifest-check: build a SWAPPED-manifest OCI image and --check it"
	@set -euo pipefail; \
	drv=`$(GUIX) repl $(LOAD) tests/manifest-image-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the swapped OCI image derivation" >&2; exit 1; }; \
	echo ">> swapped OCI image derivation: $$drv"; \
	swapped_img=`$(GUIX) build "$$drv"`; \
	echo ">> check: reproducibility of the SWAPPED OCI image derivation"; \
	$(GUIX) build --check "$$drv"; \
	echo ">> artifact check: the declared package is actually IN the built tarball"; \
	default_img=`$(GUIX) system image $(LOAD) -t docker $(SYSTEM)`; \
	probe() { \
	  listing=`tar xzOf "$$1" --wildcards '*/layer.tar' | tar tf -` \
	    || { echo "FAIL: could not read OCI archive $$1 (artifact missing or corrupt)" >&2; exit 1; }; \
	  printf '%s\n' "$$listing" | grep -c 'hello-2.12.2/bin/hello' || true; \
	}; \
	in_swapped=`probe "$$swapped_img"`; \
	in_default=`probe "$$default_img"`; \
	echo "   hello/bin/hello entries — swapped image: $$in_swapped   default image: $$in_default"; \
	test "$$in_swapped" -ge 1 || { echo "FAIL: the declared package is NOT in the built swapped tarball — the manifest reached the derivation but the exporter dropped it." >&2; exit 1; }; \
	test "$$in_default" -eq 0 || { echo "FAIL: the default image's tarball unexpectedly contains the swap package." >&2; exit 1; }; \
	echo "PASS: the declared package is present in the realized swapped image (not just the declaration) and absent from the default image."

# M10.1 bootc generation image (M10-design.md "What a generation bundle is").
# td's OCI lowering emits userspace ONLY; this builds the bootc-style image that
# makes it bootable by APPENDING a /boot layer (kernel + initrd) — see
# (system td-generation). Heavier rung (builds a system docker image + repacks),
# so it runs with the other image rungs. Self-discriminating at the artifact
# level (like manifest-check/no-guix), and it --checks reproducibility (prime
# directive 1 — this IS the new artifact):
#   • build the gen-1 + gen-2 bootc images, and `--check` BOTH bit-for-bit;
#   • crack each image's layers and assert /boot/bzImage AND /boot/initrd.cpio.gz
#     are PRESENT in the bootc image and ABSENT from the plain userspace image
#     (DRV_BASE) — the discriminator for the "made bootable" claim;
#   • assert gen-1 and gen-2 lower to DIFFERENT image derivations — each carries
#     its own generation's initrd (which mounts that generation's distinct root),
#     so the bundle is genuinely per-generation, not a shared artifact.
generation-image:
	@echo ">> generation-image: build a bootc-style generation image, --check it, crack /boot (M10.1)"
	@set -euo pipefail; \
	drvs=`$(GUIX) repl $(LOAD) tests/generation-image-drv.scm 2>/dev/null`; \
	gen1=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_GEN1=//p'`; \
	gen2=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_GEN2=//p'`; \
	base=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_BASE=//p'`; \
	nogen=`printf '%s\n' "$$drvs" | sed -n 's/^REJECTS_NO_GEN=//p'`; \
	test -n "$$gen1" -a -n "$$gen2" -a -n "$$base" || { echo "ERROR: could not lower the generation image derivations" >&2; exit 1; }; \
	echo ">> gen1 image drv: $$gen1"; \
	echo ">> gen2 image drv: $$gen2"; \
	echo ">> base userspace drv: $$base"; \
	echo ">> P1: td-generation-image rejects a config with NO generation id"; \
	test "$$nogen" = "yes" || { echo "FAIL: td-generation-image ACCEPTED a config without a generation id — it would mount the shared td-root, not a per-generation root." >&2; exit 1; }; \
	gen1_img=`$(GUIX) build "$$gen1"`; \
	gen2_img=`$(GUIX) build "$$gen2"`; \
	base_img=`$(GUIX) build "$$base"`; \
	echo ">> check: reproducibility of BOTH bootc generation images"; \
	$(GUIX) build --check "$$gen1" "$$gen2"; \
	echo ">> artifact: each bootc image carries /boot, its boot layer is WIRED into the image metadata, and the two generations' boot payloads DIFFER"; \
	tmproot=`mktemp -d`; \
	explode() { d="$$tmproot/`basename $$1`"; rm -rf "$$d"; mkdir -p "$$d"; \
	  tar xzf "$$1" -C "$$d" >/dev/null 2>&1 || { echo "FAIL: cannot read OCI image $$1" >&2; exit 1; }; echo "$$d"; }; \
	boot_layer_hex() { for l in "$$1"/*/layer.tar; do \
	  if tar tf "$$l" 2>/dev/null | grep -qE '(^|/)boot/bzImage$$'; then basename "`dirname "$$l"`"; return 0; fi; \
	  done; return 0; }; \
	probe_initrd_sha() { local img="$$1" label="$$2"; \
	  d=`explode "$$img"`; hex=`boot_layer_hex "$$d"`; \
	  test -n "$$hex" || { echo "FAIL ($$label): no layer carries /boot/bzImage — image is not bootable." >&2; exit 1; }; \
	  tar tf "$$d/$$hex/layer.tar" 2>/dev/null | grep -qE '(^|/)boot/initrd\.cpio\.gz$$' \
	    || { echo "FAIL ($$label): the boot layer is missing /boot/initrd.cpio.gz." >&2; exit 1; }; \
	  grep -q "$$hex/layer.tar" "$$d/manifest.json" \
	    || { echo "FAIL ($$label): boot layer $$hex is NOT referenced in manifest.json Layers — orphaned layer, image metadata is broken." >&2; exit 1; }; \
	  grep -q "sha256:$$hex" "$$d/config.json" \
	    || { echo "FAIL ($$label): boot layer $$hex diff_id is NOT in config.json rootfs.diff_ids — orphaned layer, image metadata is broken." >&2; exit 1; }; \
	  tar xf "$$d/$$hex/layer.tar" -C "$$d" 2>/dev/null; \
	  sha256sum "$$d"/boot/initrd.cpio.gz | cut -d' ' -f1; }; \
	dbase=`explode "$$base_img"`; \
	for l in "$$dbase"/*/layer.tar; do \
	  tar tf "$$l" 2>/dev/null | grep -qE '(^|/)boot/bzImage$$' \
	    && { echo "FAIL: the plain userspace image already carries /boot — the discriminator is broken (the appended layer was not what made it bootable)." >&2; exit 1; } || true; \
	done; \
	echo "   ok: the plain userspace image has no /boot (discriminator holds)"; \
	g1_initrd=`probe_initrd_sha "$$gen1_img" gen1`; \
	g2_initrd=`probe_initrd_sha "$$gen2_img" gen2`; \
	echo "   gen1 initrd sha256: $$g1_initrd"; \
	echo "   gen2 initrd sha256: $$g2_initrd"; \
	test -n "$$g1_initrd" -a "$$g1_initrd" != "$$g2_initrd" \
	  || { echo "FAIL: gen1 and gen2 carry BYTE-IDENTICAL initrds — the bundles are not actually per-generation (each must mount its own root), so rollback would be a no-op. A drv-name difference alone does not prove this." >&2; exit 1; }; \
	rm -rf "$$tmproot"; \
	echo "PASS: each bootc image is reproducible, carries /boot wired into its manifest+config (no orphan layer, absent from the userspace image), and the two generations' initrds differ in content — genuinely per-generation."

# 6. M7 imperative-surface removal — image-swap-only BY CONSTRUCTION (DESIGN §6).
#    M6 made image CONTENTS manifest-driven but left the imperative mutation
#    surface: the built image still ships `guix`/`guix-daemon`, so an in-image
#    `guix install` is physically possible. The typed `ship-guix?` field removes
#    it. Review showed (a) a NAME/PROPAGATION static check cannot guarantee a
#    guix-free image — guix can still arrive via a runtime reference or a renamed
#    inherited package — and (b) an OPT-IN gate is bypassable (the bare public
#    lowering stays ungated). So the real guarantee is now a CLOSURE-LEVEL gate
#    EMBEDDED in the hardened system's package set (system/td-hardening.scm
#    `guix-free-marker`, added by td-config->operating-system when ship-guix? is #f):
#    EVERY lowering builds the profile and therefore the marker, so a hardened image
#    is guix-free OR it does not build, for ANY manifest, with no opt-in to skip.
#    This rung proves that on the BARE public path, self-discriminating, against
#    explicit typed-config fixtures (triage F2 — NOT the shipped `$(SYSTEM)` target,
#    so promoting the shipped default to hardened never reddens this rung):
#      • HARDENED = bare docker image of (ship-guix? #f, base+hello): must BUILD
#        (the embedded marker certifies it guix-free); `--check` it reproducible
#        (prime directive 1 — this IS the gated artifact, so its --check covers the
#        gate too); crack its layer.tar — NO `bin/guix`/`bin/guix-daemon`.
#      • CONTROL = bare docker image of (ship-guix? #t): assert its tarball DOES
#        contain those binaries — the discriminator: if the probe stopped finding
#        guix, or the toggle stopped mattering, this reddens, so a green proves the
#        probe tells guix-ful from guix-free.
#      • ADVERSARIAL = bare docker image of (ship-guix? #f, manifest with a package
#        that keeps a RUNTIME REFERENCE to guix) — it BYPASSES the constructor's
#        name/propagation pre-filter, so guix enters the closure undetected by any
#        static check. Its BARE build MUST FAIL *at the embedded marker*
#        (verified-red half): this proves the guarantee is closure-level AND holds
#        on the ordinary public lowering, not via an opt-in. We assert both that the
#        build fails AND that it fails with the marker's own diagnostic (so an
#        unrelated build error cannot green it).
#    Artifact/closure-level (binary-absent) is STRONGER than the deferred docker-run
#    "guix install fails" runtime check (§2.3): a binary not in the image cannot run.
#    Heaviest rung → runs last (§1.3); closures are warm (base/hello/guix already built).
#    Two-step lower-then-realise (repl → guix build) for honest exit status.
no-guix:
	@echo ">> no-guix: prove ship-guix? #f is a closure-level, build-enforced guix-free guarantee (embedded, no opt-in)"
	@set -euo pipefail; \
	drvs=`$(GUIX) repl $(LOAD) tests/imperative-surface.scm 2>/dev/null`; \
	hardened_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_HARDENED=//p'`; \
	control_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_CONTROL=//p'`; \
	adversarial_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_ADVERSARIAL=//p'`; \
	shipped_gate_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_SHIPPED_GATE=//p'`; \
	svcinj_gate_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_SVCINJ_GATE=//p'`; \
	test -n "$$hardened_drv" -a -n "$$control_drv" -a -n "$$adversarial_drv" \
	     -a -n "$$shipped_gate_drv" -a -n "$$svcinj_gate_drv" \
	  || { echo "ERROR: could not lower the no-guix derivations" >&2; exit 1; }; \
	echo ">> hardened (bare, embedded-gate) image derivation: $$hardened_drv"; \
	echo ">> control  image derivation: $$control_drv"; \
	echo ">> adversarial (manifest) derivation: $$adversarial_drv"; \
	echo ">> shipped whole-system gate derivation: $$shipped_gate_drv"; \
	echo ">> service-injection gate derivation: $$svcinj_gate_drv"; \
	echo ">> guarantee: the BARE hardened lowering must BUILD (the embedded marker certifies it guix-free)"; \
	hardened_img=`$(GUIX) build "$$hardened_drv"`; \
	control_img=`$(GUIX) build "$$control_drv"`; \
	echo ">> check: reproducibility of the HARDENED (gated) artifact"; \
	$(GUIX) build --check "$$hardened_drv"; \
	echo ">> artifact check: the imperative guix surface is ABSENT from the hardened image and PRESENT in the control"; \
	probe() { \
	  listing=`tar xzOf "$$1" --wildcards '*/layer.tar' | tar tf -` \
	    || { echo "FAIL: could not read OCI archive $$1 (artifact missing or corrupt)" >&2; exit 1; }; \
	  printf '%s\n' "$$listing" | grep -Ec '/bin/guix(-daemon)?$$' || true; \
	}; \
	in_hardened=`probe "$$hardened_img"`; \
	in_control=`probe "$$control_img"`; \
	echo "   guix/guix-daemon executables — hardened image: $$in_hardened   control image: $$in_control"; \
	test "$$in_control" -ge 1 || { echo "FAIL: the ship-guix? #t control image has NO guix binary — the probe is broken or the toggle stopped mattering; the test cannot discriminate." >&2; exit 1; }; \
	test "$$in_hardened" -eq 0 || { echo "FAIL: the hardened (ship-guix? #f) image STILL contains a guix/guix-daemon binary — the imperative surface was not removed." >&2; exit 1; }; \
	echo ">> adversarial: the BARE hardened lowering of a manifest that smuggles guix past the pre-filter (runtime ref) must FAIL at the embedded marker"; \
	adv_log=`mktemp`; \
	if $(GUIX) build "$$adversarial_drv" >"$$adv_log" 2>&1; then \
	  echo "FAIL: the adversarial ship-guix? #f image BUILT on the bare public path — the embedded marker did NOT trip; guix entered the closure undetected by both the static pre-filter and the gate." >&2; \
	  tail -20 "$$adv_log" >&2; rm -f "$$adv_log"; exit 1; \
	fi; \
	if ! grep -q "STILL contains a guix" "$$adv_log"; then \
	  echo "FAIL: the adversarial build failed, but NOT at the guix-free marker (unexpected error) — cannot credit the gate:" >&2; \
	  tail -20 "$$adv_log" >&2; rm -f "$$adv_log"; exit 1; \
	fi; \
	rm -f "$$adv_log"; \
	echo "   ok: the adversarial hardened image was REJECTED at the embedded marker on the bare public path (guix-in-closure detected)"; \
	echo ">> whole-system gate: the SHIPPED system must pass the closure-level gate (it is guix-free)"; \
	$(GUIX) build "$$shipped_gate_drv" >/dev/null; \
	echo "   ok: the shipped td-system passes the whole-system guix-free gate (a guix-service regression in system/td.scm would redden this)"; \
	echo ">> service-injection: restoring guix-service-type to a hardened system must FAIL the whole-system gate (guix re-enters the SYSTEM closure, invisible to the manifest marker)"; \
	svc_log=`mktemp`; \
	if $(GUIX) build "$$svcinj_gate_drv" >"$$svc_log" 2>&1; then \
	  echo "FAIL: the service-injection system gate BUILT — guix-service-type re-introduced guix into the system closure but the whole-system gate did NOT trip. The gate does not actually scan the folded system closure." >&2; \
	  tail -20 "$$svc_log" >&2; rm -f "$$svc_log"; exit 1; \
	fi; \
	if ! grep -q "system closure STILL contains" "$$svc_log"; then \
	  echo "FAIL: the service-injection gate failed, but NOT at the whole-system guix-free gate (unexpected error) — cannot credit the gate:" >&2; \
	  tail -20 "$$svc_log" >&2; rm -f "$$svc_log"; exit 1; \
	fi; \
	rm -f "$$svc_log"; \
	echo "   ok: service-injected guix was REJECTED at the whole-system gate (the hole the manifest-only marker leaves open is closed)"; \
	echo "PASS: ship-guix? #f is a closure-level, build-enforced guarantee — (1) the embedded MARKER refuses any manifest-injected guix on every bare lowering; (2) the whole-system GATE certifies the shipped td-system guix-free and REJECTS service-injected guix (guix-service-type restored) that the marker cannot see; and the control ships the surface, proving the probes discriminate."

# 7. M8 run rung — execute the SHIPPED OCI image as a real rootless OCI container
#    (crun) and assert its userspace runs. Every rung above proves a PROPERTY of
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
#    marionette `test`/`boot-disk` rungs); here we OVERRIDE args like
#    `docker run IMG <cmd>` to drive /bin/sh. Self-discriminating: a positive run
#    (sentinel + exit 0) AND a negative control (a bogus exec must fail) — see
#    tests/run-image.sh. Heaviest behavioral rung (it unpacks the full image
#    rootfs) → runs last (§1.3).
run:
	@echo ">> run: execute the shipped OCI image as a real OCI container (crun)"
	@set -euo pipefail; \
	img=`$(GUIX) system image $(LOAD) -t docker $(SYSTEM)`; \
	test -n "$$img" || { echo "ERROR: could not build the shipped OCI image" >&2; exit 1; }; \
	echo ">> shipped OCI image: $$img"; \
	sh tests/run-image.sh "$$img"

# 8. M9.2 container-HOST rung — boot the SHIPPED base and run a Guix-built OCI APP
#    image on it with the shipped crun, as root. Where `run` (M8) ran the shipped
#    SYSTEM image's userspace, this runs a SEPARATE app image ON the booted base —
#    the container-host relationship (DESIGN §2.3 OCI app model). The app is
#    `guix pack -f docker` of GNU hello (a store path → offline, no registry); it
#    is unpacked into a runtime-bundle rootfs at build time, then crun runs it AS
#    ROOT in the guest (no rootless/userns contortions — that was M8's sandbox-only
#    concern; M9.1 made the base a host: cgroup2 mounted + crun shipped). Marionette
#    rung, so it lowers-then-realises like `test`/`boot-disk` for an honest exit
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
	    '        (cbun (run-with-store store (td-app-cgroup-bundle))))' \
	    '    (format #t "IMAGE=~a~%" (derivation-file-name img))' \
	    '    (format #t "BUNDLE=~a~%" (derivation-file-name bun))' \
	    '    (format #t "BADIMAGE=~a~%" (derivation-file-name bimg))' \
	    '    (format #t "BADBUNDLE=~a~%" (derivation-file-name bbun))' \
	    '    (format #t "CGIMAGE=~a~%" (derivation-file-name cimg))' \
	    '    (format #t "CGBUNDLE=~a~%" (derivation-file-name cbun))))' \
	  | $(GUIX) repl $(LOAD) 2>/dev/null`; \
	img=`printf '%s\n' "$$arts" | sed -n 's/^IMAGE=//p'`; \
	bun=`printf '%s\n' "$$arts" | sed -n 's/^BUNDLE=//p'`; \
	bimg=`printf '%s\n' "$$arts" | sed -n 's/^BADIMAGE=//p'`; \
	bbun=`printf '%s\n' "$$arts" | sed -n 's/^BADBUNDLE=//p'`; \
	cimg=`printf '%s\n' "$$arts" | sed -n 's/^CGIMAGE=//p'`; \
	cbun=`printf '%s\n' "$$arts" | sed -n 's/^CGBUNDLE=//p'`; \
	test -n "$$img" -a -n "$$bun" -a -n "$$bimg" -a -n "$$bbun" -a -n "$$cimg" -a -n "$$cbun" || { echo "ERROR: could not lower the app artifacts" >&2; exit 1; }; \
	echo ">> app artifacts: image=$$img bundle=$$bun"; \
	echo ">> negative-control artifacts: badimage=$$bimg badbundle=$$bbun"; \
	echo ">> cgroup artifacts (M9.3): cgimage=$$cimg cgbundle=$$cbun"; \
	$(GUIX) build "$$img" "$$bun" "$$bimg" "$$bbun" "$$cimg" "$$cbun" >/dev/null; \
	echo ">> reproducibility: guix build --check the app images + extracted bundles (good + negative + cgroup)"; \
	$(GUIX) build --check "$$img" "$$bun" "$$bimg" "$$bbun" "$$cimg" "$$cbun" >/dev/null; \
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
