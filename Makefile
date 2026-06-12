# td — the single pass/fail entry point (CLAUDE.md "The loop").
#
# `make check` runs the rung ladder. The authoritative rung list is the
# CHEAP_RUNGS/HEAVY_RUNGS pools below, which the `check:` target expands
# (CLAUDE.md "The loop"); per-rung documentation lives as a comment on each
# rule. Cheap structural rungs run serial-first, heavy rungs two at a time
# (-j2, LPT order); a red stops new rungs from spawning.
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

# Canned lower-then-realise for marionette system tests (the `test`,
# `boot-disk` and `reset` rungs; `container` lowers multiple artifacts and
# keeps its own block). Two steps on purpose: `guix repl` reading a script
# from STDIN always exits 0 (it swallows the script's exit code), so building
# the test there would make a FAILED test look green. Instead: (1) lower the
# monadic test value to a derivation file name via repl, then (2) realise it
# with `guix build`, whose exit status is honest and which streams the
# marionette log so failures are visible.
#   $(1) = test module, e.g. (tests boot)
#   $(2) = system-test variable, e.g. %test-td-boot
#   $(3) = label for messages, e.g. boot
define realise-system-test
	@drv=`printf '%s\n' \
	    '(use-modules (guix) (gnu tests) $(1))' \
	    '(with-store store' \
	    '  (set-build-options store #:use-substitutes? #f #:offload? #f)' \
	    '  (format #t "DRV=~a~%"' \
	    '          (derivation-file-name' \
	    '           (run-with-store store (system-test-value $(2))))))' \
	  | $(GUIX) repl $(LOAD) 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the $(3) test derivation" >&2; exit 1; }; \
	echo ">> realise $(3) test derivation: $$drv"; \
	$(GUIX) build "$$drv"
endef

# Bare `make` runs the in-sandbox loop, never the sandbox wrapper — guards
# against `container-check` (which calls ./check.sh) being the default goal and
# recursing into nested containers.
.DEFAULT_GOAL := check

# The 22 rungs, in the two pools the bounded-parallel loop schedules from.
# ADDING A RUNG: put it in exactly ONE pool below — .PHONY, the `check` target,
# the serial chain, and the heavy gate are all DERIVED from these two
# variables, so the lists cannot drift apart (review finding: they used to be
# three hand-kept copies).
#
# CHEAP_RUNGS are the sub-5s structural rungs; their list order IS their
# strict serial execution order (a generated order-only chain below), so a
# syntax error or differential regression reds the loop before any VM boots
# or tarball repacks.
#
# HEAVY_RUNGS run at most two at a time under check.sh's `make -j2` (DESIGN
# §7.3 resource note: more concurrent VMs may thrash; empirically the daemon
# overlaps two client builds, 17s->10s on the place trees). They are listed
# LONGEST-FIRST (LPT packing): under -j2 make starts them in list order, and
# seeding the slots with the longest rungs lets the short ones fill the gaps
# instead of leaving a long rung to run alone at the end (measured: the naive
# order left `container` solo for its full 71s). RE-MEASURE AND RE-SORT this
# list whenever a rung is added or the full-check wall time drifts well past
# the recorded 341s (-j2 floor with 18 rungs, 2026-06-10; per-rung numbers:
# plan/loop-latency.md "Measurement log"). `rollback` is seeded first on
# M10.3's judgment, not yet individually measured; `rootless` is slotted after
# `container` on its measured solo run (36s incl. sandbox setup,
# plan/rootless-builder.md); `oci-load` after `rootless` on its measured solo
# run (plan/oci-load.md — skopeo passes are seconds; the gunzip/regzip of the
# negative control dominates). `td-builder` (S1) is slotted late on judgment,
# not yet individually measured — its cost is a single warm-store Rust compile
# plus a --check rebuild; RE-MEASURE and RE-SORT once it has run. A stale order
# only costs latency, never correctness.
#
# NOTHING is removed, loosened, or skipped by the parallelism: all rungs must
# still pass, and make (run without -k) stops spawning new rungs after a
# failure — a red still short-circuits the loop. Order-only (|) prerequisites,
# so a plain serial `make -j1 check` behaves exactly as before.
CHEAP_RUNGS := eval diff typed-coverage oci-diff manifest-diff generation-diff
HEAVY_RUNGS := rollback generation-image no-guix manifest-check oci container rootless oci-load reset test place build boot-disk td-builder run offline

.PHONY: check container-check $(CHEAP_RUNGS) $(HEAVY_RUNGS)

# The hermetic, offline, self-contained entry point (DESIGN §1.1/§1.4). Plain
# `make check` assumes you are ALREADY inside the right `guix shell -C` sandbox;
# `make container-check` (or ./check.sh) sets that sandbox up for you. Prefer it.
container-check:
	@./check.sh

check: $(CHEAP_RUNGS) $(HEAVY_RUNGS)

# Generated ordering graph (do not hand-edit): chain each cheap rung
# order-only on its predecessor, and gate every heavy rung on the last cheap
# rung.
chain-prev :=
$(foreach r,$(CHEAP_RUNGS),$(eval $(if $(chain-prev),$(r): | $(chain-prev)))$(eval chain-prev := $(r)))
$(HEAVY_RUNGS): | $(lastword $(CHEAP_RUNGS))

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
	$(call realise-system-test,(tests boot),%test-td-boot,boot)

# 3b. Disk-image boot (triage #2) — boot the qcow2 through its GRUB bootloader
#     (not the direct-kernel VM the `test` rung uses), so the bootloader,
#     partition table and disk image are actually exercised. Same honest two-step
#     lower-then-realise as `test`. Heavier (builds a second full image + boots
#     it), so it runs after the cheap rungs.
boot-disk:
	@echo ">> boot-disk: boot the qcow2 disk through GRUB + assert kernel"
	$(call realise-system-test,(tests boot),%test-td-disk-boot,disk-boot)

# 3c. Ephemerality of the CoW reset (loop-latency; DESIGN §1.5). Boots the SAME
#     instrumented qcow2 derivation as boot-disk (cache hit, no extra image
#     build) three times on explicit qcow2 overlays: dirt written on overlay A,
#     dirt STILL THERE on reused overlay A (negative control — writes really
#     persist without a reset), dirt GONE on fresh overlay B (the reset). Makes
#     the loop's fresh-state-per-test guarantee an assertion instead of an
#     implicit property of qemu flags, so any future cycle-time change that
#     leaks guest state across boots goes red here. Same honest two-step
#     lower-then-realise as `test`/`boot-disk`.
reset:
	@echo ">> reset: CoW overlay reset discards dirtied guest state (ephemerality)"
	$(call realise-system-test,(tests reset),%test-td-reset,reset)

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
	echo ">> validate artifacts (structured: guile-json metadata + guile-zlib initrd)"; \
	TD_GEN1_IMG="$$gen1_img" TD_GEN2_IMG="$$gen2_img" TD_BASE_IMG="$$base_img" \
	  $(GUIX) repl $(LOAD) tests/generation-image-check.scm

# oci-load (side-track, deferred from M10.1; plan/oci-load.md). The shipped
# images must be consumable by an INDEPENDENT OCI implementation, not just our
# own placer (`place`) and runtime (`run`). Vehicle: skopeo, chosen by the M8
# probe discipline — 0 drvs to build on the warm store vs umoci 113 and podman
# 1238 + 290 cold fetches (rejected at M8); resolved via `$(GUIX) build` so
# check.sh's package list is untouched. For BOTH the plain td image and the
# gen-1 bootc generation image (drvs shared with `oci`/`generation-image`, so
# the marginal cost is the skopeo pass, not a rebuild):
#   • `skopeo copy docker-archive:… oci:…` — the foreign stack parses the
#     archive and verifies every blob digest while writing the CANONICAL OCI
#     LAYOUT, the §2.7 identity carrier;
#   • assert `skopeo inspect` yields a `sha256:` manifest digest from that
#     layout (the registry-addressable identity M12 signs).
# NEGATIVE CONTROL, in-rung: the gen-1 archive with ONE byte incremented inside
# the inner layer.tar must be REJECTED with a digest mismatch — proves the
# green leg is a real integrity check, not mere unpacking. The corruptor
# increments (mod 256) the byte at the midpoint, so the write can never be a
# no-op, and the midpoint of the outer tar lies inside the dominant layer.tar
# blob. `--insecure-policy` disables only signature *trust policy* (M12's
# territory, no keys exist yet); blob-digest integrity stays enforced — which
# is exactly what the control proves. Scratch lives in
# $(CURDIR)/.oci-load-scratch (disk, not the sandbox tmpfs — the rootless
# lesson: layouts + the decompressed archive are several GB); kept on red for
# triage, removed on green.
oci-load:
	@echo ">> oci-load: foreign OCI implementation (skopeo) loads the shipped images"
	@set -euo pipefail; \
	skopeo=`$(GUIX) build skopeo`/bin/skopeo; \
	plain_img=`$(GUIX) system image $(LOAD) -t docker $(SYSTEM)`; \
	gen1=`$(GUIX) repl $(LOAD) tests/generation-image-drv.scm 2>/dev/null | sed -n 's/^DRV_GEN1=//p'`; \
	test -n "$$gen1" || { echo "ERROR: could not lower the gen-1 bootc image derivation" >&2; exit 1; }; \
	gen1_img=`$(GUIX) build "$$gen1"`; \
	work="$(CURDIR)/.oci-load-scratch"; rm -rf "$$work"; mkdir -p "$$work"; \
	for leg in plain:$$plain_img gen1:$$gen1_img; do \
	  name=$${leg%%:*}; img=$${leg#*:}; \
	  echo ">> skopeo copy docker-archive -> oci layout ($$name): $$img"; \
	  "$$skopeo" --tmpdir "$$work" copy --insecure-policy "docker-archive:$$img" "oci:$$work/layout-$$name:td" >/dev/null; \
	  digest=`"$$skopeo" --tmpdir "$$work" inspect --format '{{.Digest}}' "oci:$$work/layout-$$name:td"`; \
	  case "$$digest" in \
	    sha256:*) echo "   manifest digest ($$name): $$digest";; \
	    *) echo "FAIL: no manifest digest from the $$name OCI layout (got: '$$digest')" >&2; exit 1;; \
	  esac; \
	done; \
	echo ">> negative control: a corrupted layer must be REJECTED"; \
	gunzip -c "$$gen1_img" > "$$work/bad.tar"; \
	off=$$(( `stat -c %s "$$work/bad.tar"` / 2 )); \
	b=`od -An -tu1 -j $$off -N1 "$$work/bad.tar" | tr -d ' '`; \
	printf "\\$$(printf '%03o' $$(( (b + 1) % 256 )))" \
	  | dd of="$$work/bad.tar" bs=1 seek=$$off count=1 conv=notrunc status=none; \
	gzip -1 "$$work/bad.tar"; \
	if "$$skopeo" --tmpdir "$$work" copy --insecure-policy "docker-archive:$$work/bad.tar.gz" \
	     "oci:$$work/layout-bad:bad" >/dev/null 2>"$$work/bad.err"; then \
	  echo "FAIL: skopeo ACCEPTED a deliberately corrupted image — the load is not an integrity check." >&2; \
	  cat "$$work/bad.err" >&2; \
	  exit 1; \
	fi; \
	grep -qi 'digest did not match' "$$work/bad.err" \
	  || { echo "FAIL: corrupted image was rejected, but NOT with a digest mismatch:" >&2; \
	       cat "$$work/bad.err" >&2; exit 1; }; \
	rm -rf "$$work"; \
	echo "PASS: foreign load green for plain + gen-1 images; corrupted layer rejected (digest mismatch)."

# M10.2 guix-free placer (M10-design.md step 3, "Place"). The deployment side:
# a POSIX shell tool (system/td-place.sh) that runs ON THE TARGET — which has NO
# guix. Driven by the OCI manifest (not a blind layer scan), it: verifies the
# image's embedded identity (boot/td-identity) matches the --generation/--root-label
# it is placed as; APPLIES the userspace layers into that generation's own root,
# staged as roots/td/gen-N/root.tar (so the bare-label root=td-root-gen-N refers to a root
# that exists — M10.3 turns it into a labeled fs); extracts /boot per-generation;
# prunes to --keep (>=1); and regenerates a per-generation GRUB menu. Each
# generation is staged + validated then atomically swapped in, so a corrupt image
# never destroys the generation already installed. This rung exercises it
# hermetically (system/td-place.scm): it builds the per-generation bootc images
# with Guix (the M10.1 oracle) and runs the placer over them inside a derivation
# whose builder PATH is ONLY base tools, NO guix — so a successful build PROVES the
# placer is guix-free by construction (the same "absent → cannot be used" guarantee
# as `no-guix`), and `--check` proves the placed target tree reproducible. The
# deployment behavior is tested against the artifact (M10-design.md decision 2),
# not diffed against a Guix component it lacks: tests/place-check.scm cracks the
# tree and asserts each present generation is placed with its own kernel/initrd,
# an identity recording the artifact's sha256 (image-digest=, M12 §2.7 —
# value-checked against the real artifacts via TD_IMAGES),
# its applied root content, and a menuentry that selects its OWN root and no
# other's (per-entry, not block-wide), the user grub.cfg preamble survives, and
# (the prune scenario) the oldest generation's boot dir, root content AND menu
# entry are gone. Two scenarios: PLACE (gens 1,2 keep 10 — no prune) and PRUNE
# (gens 1,2,3 keep 2 — gen 1 dropped). Creating the labeled fs from the staged
# root.tar + the full boot+rollback is M10.3.
place:
	@echo ">> place: guix-free placer extracts /boot + writes a per-generation GRUB menu, prunes old generations (M10.2)"
	@set -euo pipefail; \
	drvs=`$(GUIX) repl $(LOAD) tests/place-drv.scm 2>/dev/null`; \
	place_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_PLACE=//p'`; \
	prune_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_PRUNE=//p'`; \
	img1=`printf '%s\n' "$$drvs" | sed -n 's/^IMG_1=//p'`; \
	img2=`printf '%s\n' "$$drvs" | sed -n 's/^IMG_2=//p'`; \
	img3=`printf '%s\n' "$$drvs" | sed -n 's/^IMG_3=//p'`; \
	test -n "$$place_drv" -a -n "$$prune_drv" || { echo "ERROR: could not lower the placer tree derivations" >&2; exit 1; }; \
	test -n "$$img1" -a -n "$$img2" -a -n "$$img3" || { echo "ERROR: could not lower the generation image artifact paths" >&2; exit 1; }; \
	echo ">> place  tree derivation (gens 1,2 keep 10): $$place_drv"; \
	echo ">> prune  tree derivation (gens 1,2,3 keep 2): $$prune_drv"; \
	place_tree=`$(GUIX) build "$$place_drv"`; \
	prune_tree=`$(GUIX) build "$$prune_drv"`; \
	echo ">> check: reproducibility of BOTH placed target trees"; \
	$(GUIX) build --check "$$place_drv" "$$prune_drv"; \
	echo ">> validate PLACE tree (gens 1,2 present, none pruned)"; \
	TD_PLACED="$$place_tree" TD_PRESENT="1 2" TD_ABSENT="" \
	TD_IMAGES="1=$$img1 2=$$img2" \
	  $(GUIX) repl $(LOAD) tests/place-check.scm; \
	echo ">> validate PRUNE tree (gens 2,3 present, gen 1 pruned)"; \
	TD_PLACED="$$prune_tree" TD_PRESENT="2 3" TD_ABSENT="1" \
	TD_IMAGES="2=$$img2 3=$$img3" \
	  $(GUIX) repl $(LOAD) tests/place-check.scm

# M10.3 manual rollback (M10-design.md step 5, "Roll back"; the DESIGN §7.1
# acceptance test). End-to-end: the guix-free placer's output — live labeled
# per-generation root filesystems (--mkfs) + the managed GRUB menu — is
# assembled into a real MBR/GRUB disk (system/td-disk.scm), and the marionette
# test (tests/rollback.scm) boots ONE persistent qcow2 overlay of it TWICE:
# generation 2 (the GRUB default) is asserted three independent ways (cmdline
# bare-label root=, mounted-root-IS-the-labeled-filesystem, /run/current-system ==
# gen-2's system path — the placer's gnu.system wiring), the manual rollback
# act writes `set default=td-gen-1` into the boot partition's td/default.cfg
# (the hook the managed block sources) plus a persistence sentinel, the guest
# reboots cleanly, and generation 1 is asserted the same three ways — with the
# sentinel, the selection, gen-2's placed files and BOTH menu entries proven to
# have survived the reboot (persistent placed state; rolling back never
# destroys the newer generation). Before booting: `--check` both new artifacts
# (the mkfs tree and the assembled disk — prime directive 1) and validate the
# tree with tests/place-check.scm in mkfs mode (superblock label/UUID, search
# line). Two-step lower-then-realise for the marionette derivation, as in
# `test`/`boot-disk` (honest exit status).
rollback:
	@echo ">> rollback: boot gen 2, roll back to gen 1 via the GRUB menu, assert identity + persistence (M10.3)"
	@set -euo pipefail; \
	drvs=`$(GUIX) repl $(LOAD) tests/rollback-drv.scm 2>/dev/null`; \
	tree_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_TREE=//p'`; \
	disk_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_DISK=//p'`; \
	test -n "$$tree_drv" -a -n "$$disk_drv" || { echo "ERROR: could not lower the rollback derivations" >&2; exit 1; }; \
	echo ">> placed tree (mkfs) derivation: $$tree_drv"; \
	echo ">> rollback disk derivation:      $$disk_drv"; \
	tree=`$(GUIX) build "$$tree_drv"`; \
	disk=`$(GUIX) build "$$disk_drv"`; \
	echo ">> check: reproducibility of the mkfs placed tree AND the assembled disk"; \
	$(GUIX) build --check "$$tree_drv" "$$disk_drv"; \
	echo ">> validate the mkfs tree (live labeled roots via superblock, boot wiring, search line)"; \
	TD_PLACED="$$tree" TD_PRESENT="1 2" TD_ABSENT="" TD_MKFS=1 TD_BOOT_LABEL=td-boot \
	  $(GUIX) repl $(LOAD) tests/place-check.scm; \
	drv=`printf '%s\n' \
	    '(use-modules (guix) (gnu tests) (tests rollback))' \
	    '(with-store store' \
	    '  (set-build-options store #:use-substitutes? #f #:offload? #f)' \
	    '  (format #t "DRV=~a~%"' \
	    '          (derivation-file-name' \
	    '           (run-with-store store (system-test-value %test-td-rollback)))))' \
	  | $(GUIX) repl $(LOAD) 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the rollback test derivation" >&2; exit 1; }; \
	echo ">> realise rollback test derivation: $$drv"; \
	$(GUIX) build "$$drv"

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

# td-builder S1 toolchain probe + S2 NAR differential (DESIGN §7.1 side-track;
# plan/td-builder.md). The growing rung of the first Guix-component replacement
# (§2.5 discipline) — each sub-task adds a leg, none is ever removed:
#   • S1: lower the td-builder package to a drv (tests/td-builder-drv.scm),
#     build it offline, `guix build --check` it bit-for-bit (prime directive 1;
#     --check re-runs the compile, so a toolchain regression reds the loop),
#     RUN the binary and assert its sentinel (the toolchain produced a WORKING
#     executable — stronger than "cargo build exited 0"), and record closure
#     size + compile wall-clock (§1.3). The crate's unit tests (FIPS SHA-256
#     vectors, NAR framing/sort) also run inside the build (#:tests? #t).
#   • S2: NAR DIFFERENTIAL — td-builder's own NAR serializer + SHA-256
#     (`nar-hash`) must agree with the hash the DAEMON recorded in its DB
#     (query-path-info via tests/td-builder-nar.scm, printing NAR=<path> <hash>
#     pairs) for (1) a constructed fixture covering every node type and
#     framing edge (executable bit, dangling symlink, empty file/dir,
#     codepoint-order sort stress, pad-to-8 content lengths) and (2)
#     td-builder's own output. This is open question 2 settled by test: the
#     serialization the eventual builder registers outputs with is bit-for-bit
#     the daemon's. Verified-red (driven before this leg may land):
#     ordering/padding defects in nar.rs each red it — evidence in
#     plan/td-builder.md.
#   • S3: BUILD DIFFERENTIAL — td-builder parses the ATerm drv, executes its
#     builder in a fresh user namespace (uid 30001, staged store rbind, the
#     daemon's env contract — plan/td-builder.md Q4) and registers the output
#     (v1 record — Q3). Asserted against the daemon, which builds the SAME
#     deterministic drv (tests/td-builder-s3-drvs.scm): same store path,
#     NAR hash equal to the daemon's RECORDED hash, NAR size, references set
#     (an input ref + a self-ref — the scan must find both) and deriver all
#     equal; plus the rootless rung's isolation assert on a separate
#     namespace-sensitive probe drv (built td-side only — its output records
#     uid_map and can never be a differential subject).
#   • S4: SYSTEM-IMAGE DIFFERENTIAL — the §7.1 acceptance subject: td-builder
#     rebuilds the `build` rung's qcow2 image drv itself
#     (tests/td-builder-s4-drv.scm prints the oracle facts the root daemon
#     recorded when it built the SAME drv) and must register equal fields at
#     the same path — store path, NAR hash (recorded AND independently
#     re-hashed), NAR size, references set (compared even if empty) and
#     deriver. This is what forces the sandbox past S3's minimum: the image
#     builder is a real multi-process Guile build (mke2fs/genimage tree) that
#     honestly reds on any missing piece of the daemon's chroot contract.
# OFFLINE PRECONDITION (DESIGN §5): the pinned Rust closure must be warm in the
# host store — the loop fetches nothing. Two-step lower-then-realise (repl ->
# guix build) for an honest exit status, as in the other rungs.
td-builder:
	@echo ">> td-builder: reproducible offline build (S1) + NAR differential (S2) + build differential (S3) + system-image differential (S4)"
	@set -euo pipefail; \
	drv=`$(GUIX) repl $(LOAD) tests/td-builder-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the td-builder derivation" >&2; exit 1; }; \
	echo ">> td-builder derivation: $$drv"; \
	start=`date +%s`; \
	out=`$(GUIX) build "$$drv"`; \
	elapsed=$$(( `date +%s` - start )); \
	test -n "$$out" || { echo "ERROR: the td-builder build produced no output path" >&2; exit 1; }; \
	echo ">> check: reproducibility of the td-builder binary"; \
	$(GUIX) build --check "$$drv"; \
	echo ">> run: the compiled binary must print its sentinel"; \
	"$$out/bin/td-builder" | grep -Eq '^td-builder [0-9.]+ ok$$' \
	  || { echo "FAIL: the compiled td-builder did not print its sentinel (or exited nonzero) — the toolchain did not produce a working binary." >&2; exit 1; }; \
	echo ">> S2: NAR differential — td-builder nar-hash vs the daemon's recorded hash"; \
	pairs=`$(GUIX) repl $(LOAD) tests/td-builder-nar.scm 2>/dev/null | sed -n 's/^NAR=//p'`; \
	test -n "$$pairs" || { echo "ERROR: could not compute the oracle NAR pairs (tests/td-builder-nar.scm)" >&2; exit 1; }; \
	n=0; \
	while read -r p expect; do \
	  test -n "$$p" -a -n "$$expect" || { echo "ERROR: malformed oracle pair: '$$p $$expect'" >&2; exit 1; }; \
	  have=`"$$out/bin/td-builder" nar-hash "$$p"` \
	    || { echo "FAIL: td-builder nar-hash failed on $$p" >&2; exit 1; }; \
	  test "$$have" = "sha256:$$expect" \
	    || { echo "FAIL: NAR hash mismatch for $$p" >&2; \
	         echo "      td-builder: $$have" >&2; \
	         echo "      daemon    : sha256:$$expect" >&2; exit 1; }; \
	  echo "   nar ok ($$have): $$p"; \
	  n=$$((n + 1)); \
	done <<< "$$pairs"; \
	test "$$n" -ge 2 || { echo "FAIL: expected at least 2 oracle NAR pairs (fixture + td-builder output), got $$n" >&2; exit 1; }; \
	echo ">> S3: drv parse + sandboxed userns build differential vs the daemon"; \
	scratch="$(CURDIR)/.td-builder-scratch"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	$(GUIX) repl $(LOAD) tests/td-builder-s3-drvs.scm 2>/dev/null > "$$scratch/s3.txt"; \
	diff_drv=`sed -n 's/^DIFF_DRV=//p' "$$scratch/s3.txt"`; \
	diff_out=`sed -n 's/^DIFF_OUT=//p' "$$scratch/s3.txt"`; \
	diff_hash=`sed -n 's/^DIFF_HASH=//p' "$$scratch/s3.txt"`; \
	diff_narsize=`sed -n 's/^DIFF_NARSIZE=//p' "$$scratch/s3.txt"`; \
	diff_deriver=`sed -n 's/^DIFF_DERIVER=//p' "$$scratch/s3.txt"`; \
	probe_drv=`sed -n 's/^PROBE_DRV=//p' "$$scratch/s3.txt"`; \
	probe_out=`sed -n 's/^PROBE_OUT=//p' "$$scratch/s3.txt"`; \
	test -n "$$diff_drv" -a -n "$$diff_out" -a -n "$$diff_hash" -a -n "$$diff_narsize" \
	     -a -n "$$diff_deriver" -a -n "$$probe_drv" -a -n "$$probe_out" \
	  || { echo "ERROR: could not lower the S3 drvs (tests/td-builder-s3-drvs.scm)" >&2; exit 1; }; \
	{ sed -n 's/^DIFF_INPUT=//p;s/^PROBE_INPUT=//p' "$$scratch/s3.txt"; \
	  printf '%s\n' "$$diff_drv" "$$probe_drv"; } \
	  | xargs $(GUIX) gc -R | sort -u > "$$scratch/paths.txt"; \
	echo "   staged closure: $$(wc -l < "$$scratch/paths.txt") store items"; \
	"$$out/bin/td-builder" drv-parse "$$diff_drv" > /dev/null \
	  || { echo "FAIL: td-builder drv-parse rejected the diff drv $$diff_drv" >&2; exit 1; }; \
	echo "   isolation probe: the build must run in a fresh user namespace"; \
	"$$out/bin/td-builder" build "$$probe_drv" "$$scratch/paths.txt" "$$scratch/probe" > /dev/null \
	  || { echo "FAIL: td-builder could not build the isolation probe drv" >&2; exit 1; }; \
	map="$$scratch/probe/newstore/$${probe_out#/gnu/store/}/uid_map"; \
	test -s "$$map" || { echo "FAIL: the isolation probe recorded an empty uid_map" >&2; exit 1; }; \
	echo "   uid_map seen by the td-builder sandbox:"; sed 's/^/     /' "$$map"; \
	map_lines=`wc -l < "$$map"`; read -r map_first map_rest < "$$map"; \
	if [ "$$map_lines" -ne 1 ] || [ "$$map_first" != "30001" ]; then \
	  echo "FAIL: the td-builder build's uid_map is not a fresh per-build user" >&2; \
	  echo "      namespace mapping with the daemon's guest uid (expected the" >&2; \
	  echo "      single entry '30001 <host> 1' — build.cc defaultGuestUID; a" >&2; \
	  echo "      leading 0 means no/inherited namespace, any other uid breaks" >&2; \
	  echo "      the Q4 contract)." >&2; exit 1; \
	fi; \
	echo "   differential: td-builder rebuild vs the daemon's recorded facts"; \
	"$$out/bin/td-builder" build "$$diff_drv" "$$scratch/paths.txt" "$$scratch/diff" > "$$scratch/diff-build.txt" \
	  || { echo "FAIL: td-builder could not build the diff drv $$diff_drv" >&2; exit 1; }; \
	grep -qx "OUT=out $$diff_out" "$$scratch/diff-build.txt" \
	  || { echo "FAIL: store-path mismatch: td-builder reported '$$(cat "$$scratch/diff-build.txt")', the daemon built $$diff_out" >&2; exit 1; }; \
	reg="$$scratch/diff/registration"; \
	test -s "$$reg" || { echo "FAIL: td-builder wrote no registration record" >&2; exit 1; }; \
	grep -qx "path $$diff_out" "$$reg" \
	  || { echo "FAIL: registration path mismatch (see record below) vs $$diff_out" >&2; cat "$$reg" >&2; exit 1; }; \
	grep -qx "nar-hash sha256:$$diff_hash" "$$reg" \
	  || { echo "FAIL: NAR hash mismatch — registration '$$(sed -n 's/^nar-hash //p' "$$reg")' vs daemon 'sha256:$$diff_hash'" >&2; exit 1; }; \
	grep -qx "nar-size $$diff_narsize" "$$reg" \
	  || { echo "FAIL: NAR size mismatch — registration '$$(sed -n 's/^nar-size //p' "$$reg")' vs daemon '$$diff_narsize'" >&2; exit 1; }; \
	grep -qx "deriver $$diff_deriver" "$$reg" \
	  || { echo "FAIL: deriver mismatch — registration '$$(sed -n 's/^deriver //p' "$$reg")' vs daemon '$$diff_deriver'" >&2; exit 1; }; \
	sed -n 's/^DIFF_REF=//p' "$$scratch/s3.txt" > "$$scratch/refs.oracle"; \
	sed -n 's/^reference //p' "$$reg" > "$$scratch/refs.td"; \
	test -s "$$scratch/refs.oracle" \
	  || { echo "ERROR: the oracle recorded NO references for the diff drv — the fixture lost its discriminating refs" >&2; exit 1; }; \
	test "$$(cat "$$scratch/refs.oracle")" = "$$(cat "$$scratch/refs.td")" \
	  || { echo "FAIL: references set mismatch:" >&2; \
	       echo "      daemon recorded:" >&2; sed 's/^/        /' "$$scratch/refs.oracle" >&2; \
	       echo "      td-builder registered:" >&2; sed 's/^/        /' "$$scratch/refs.td" >&2; exit 1; }; \
	rehash=`"$$out/bin/td-builder" nar-hash "$$scratch/diff/newstore/$${diff_out#/gnu/store/}"`; \
	test "$$rehash" = "sha256:$$diff_hash" \
	  || { echo "FAIL: independent re-hash of the on-disk rebuild gives $$rehash, the daemon recorded sha256:$$diff_hash" >&2; exit 1; }; \
	echo "   rebuild equal: store path, NAR hash (registered + re-hashed), size, references (input + self), deriver"; \
	echo ">> S4: system-image differential — td-builder rebuilds the build rung's qcow2 drv"; \
	img_drv=`$(GUIX) system image $(LOAD) -t $(IMGTYPE) -d $(SYSTEM)`; \
	test -n "$$img_drv" || { echo "ERROR: could not lower the image derivation" >&2; exit 1; }; \
	echo "   target image drv: $$img_drv"; \
	img_oracle=`$(GUIX) build "$$img_drv"`; \
	test -n "$$img_oracle" || { echo "ERROR: the oracle image build produced no output path" >&2; exit 1; }; \
	TD_IMAGE_DRV="$$img_drv" $(GUIX) repl $(LOAD) tests/td-builder-s4-drv.scm 2>/dev/null > "$$scratch/s4.txt"; \
	img_out=`sed -n 's/^IMG_OUT=//p' "$$scratch/s4.txt"`; \
	img_hash=`sed -n 's/^IMG_HASH=//p' "$$scratch/s4.txt"`; \
	img_narsize=`sed -n 's/^IMG_NARSIZE=//p' "$$scratch/s4.txt"`; \
	img_deriver=`sed -n 's/^IMG_DERIVER=//p' "$$scratch/s4.txt"`; \
	test -n "$$img_out" -a -n "$$img_hash" -a -n "$$img_narsize" -a -n "$$img_deriver" \
	  || { echo "ERROR: could not read the S4 oracle facts (tests/td-builder-s4-drv.scm)" >&2; exit 1; }; \
	test "$$img_out" = "$$img_oracle" \
	  || { echo "ERROR: lowered image output ($$img_out) != realized oracle output ($$img_oracle)" >&2; exit 1; }; \
	{ sed -n 's/^IMG_INPUT=//p' "$$scratch/s4.txt"; printf '%s\n' "$$img_drv"; } \
	  | xargs $(GUIX) gc -R | sort -u > "$$scratch/s4-paths.txt"; \
	echo "   staged closure: $$(wc -l < "$$scratch/s4-paths.txt") store items"; \
	"$$out/bin/td-builder" build "$$img_drv" "$$scratch/s4-paths.txt" "$$scratch/s4" > "$$scratch/s4-build.txt" \
	  || { echo "FAIL: td-builder could not build the image drv $$img_drv" >&2; exit 1; }; \
	grep -qx "OUT=out $$img_out" "$$scratch/s4-build.txt" \
	  || { echo "FAIL: store-path mismatch: td-builder reported '$$(cat "$$scratch/s4-build.txt")', the daemon built $$img_out" >&2; exit 1; }; \
	s4reg="$$scratch/s4/registration"; \
	test -s "$$s4reg" || { echo "FAIL: td-builder wrote no registration record for the image" >&2; exit 1; }; \
	grep -qx "path $$img_out" "$$s4reg" \
	  || { echo "FAIL: image registration path mismatch (see record below) vs $$img_out" >&2; cat "$$s4reg" >&2; exit 1; }; \
	grep -qx "nar-hash sha256:$$img_hash" "$$s4reg" \
	  || { echo "FAIL: image NAR hash mismatch — registration '$$(sed -n 's/^nar-hash //p' "$$s4reg")' vs daemon 'sha256:$$img_hash'" >&2; exit 1; }; \
	grep -qx "nar-size $$img_narsize" "$$s4reg" \
	  || { echo "FAIL: image NAR size mismatch — registration '$$(sed -n 's/^nar-size //p' "$$s4reg")' vs daemon '$$img_narsize'" >&2; exit 1; }; \
	grep -qx "deriver $$img_deriver" "$$s4reg" \
	  || { echo "FAIL: image deriver mismatch — registration '$$(sed -n 's/^deriver //p' "$$s4reg")' vs daemon '$$img_deriver'" >&2; exit 1; }; \
	sed -n 's/^IMG_REF=//p' "$$scratch/s4.txt" > "$$scratch/s4-refs.oracle"; \
	sed -n 's/^reference //p' "$$s4reg" > "$$scratch/s4-refs.td"; \
	test "$$(cat "$$scratch/s4-refs.oracle")" = "$$(cat "$$scratch/s4-refs.td")" \
	  || { echo "FAIL: image references set mismatch:" >&2; \
	       echo "      daemon recorded:" >&2; sed 's/^/        /' "$$scratch/s4-refs.oracle" >&2; \
	       echo "      td-builder registered:" >&2; sed 's/^/        /' "$$scratch/s4-refs.td" >&2; exit 1; }; \
	img_rehash=`"$$out/bin/td-builder" nar-hash "$$scratch/s4/newstore/$${img_out#/gnu/store/}"`; \
	test "$$img_rehash" = "sha256:$$img_hash" \
	  || { echo "FAIL: independent re-hash of the on-disk image rebuild gives $$img_rehash, the daemon recorded sha256:$$img_hash" >&2; exit 1; }; \
	echo "   image rebuild equal: store path, NAR hash (registered + re-hashed), size, references, deriver"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; \
	echo ">> closure size:"; $(GUIX) size "$$out" | tail -n1; \
	echo "   compile wall-clock: $${elapsed}s (first run; warm store thereafter)"; \
	echo "PASS: reproducible offline build (S1); NAR serialization bit-for-bit equal to the daemon's recorded hashes across $$n items (S2); the userns sandbox rebuild registers the daemon's exact facts at the same store path and builds in a fresh user namespace (S3); td-builder rebuilds the SYSTEM IMAGE drv itself, daemon-equal on every recorded field (S4)."

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

# 9. offline-isolation sandbox probe (plan/offline-isolation.md S1). The
#    hermeticity clause says an UNDECLARED fetch — network access from a
#    non-fixed-output builder — must be impossible; until now that was an
#    assumed property of guix-daemon's sandbox, never an asserted rung. This
#    realises tests/offline-drv.scm's DRV_SANDBOX probe: a regular derivation
#    whose builder must see ONLY `lo` in /proc/net/dev and whose TCP egress
#    attempt must raise — i.e. the deliberate undeclared fetch demonstrably
#    fails. Then `guix build --check` re-runs the builder, so the assertions
#    RE-EXECUTE every loop (and the probe is proven reproducible, prime
#    directive 1) — a daemon regression (e.g. --disable-chroot) reds this rung
#    on the next check, not just on a cold store. Self-discriminating across
#    contexts: check.sh's host-side control proves the SAME /proc/net/dev
#    mechanism reports non-lo interfaces where network IS present, and the
#    fixed-output twin (DRV_DAEMON, wired in at S2) is the same builder body
#    failing red in a network-visible netns (verified-red evidence in
#    plan/offline-isolation.md). Cheapest heavy rung (one tiny local build) →
#    listed last (LPT).
offline:
	@echo ">> offline: an undeclared (non-fixed-output) network fetch must FAIL in the build sandbox"
	@set -euo pipefail; \
	drvs=`$(GUIX) repl $(LOAD) tests/offline-drv.scm 2>/dev/null`; \
	sandbox_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_SANDBOX=//p'`; \
	test -n "$$sandbox_drv" || { echo "ERROR: could not lower the offline probe derivations" >&2; exit 1; }; \
	echo ">> sandbox probe derivation: $$sandbox_drv"; \
	$(GUIX) build "$$sandbox_drv"; \
	echo ">> re-run + reproducibility: --check forces the sandbox probe assertions to re-execute"; \
	$(GUIX) build --check "$$sandbox_drv"; \
	echo "PASS: a non-fixed-output builder has no network — loopback-only netns, egress raises (re-checked this run)."
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
#        reds the rung. The probe is an instrument, never `--check`ed, and its
#        output exists only in the discarded scratch store (it must stay
#        INVALID in the real store — the guard reds if it ever becomes valid);
#      • the differential: rootless `guix build --check` of the SAME image drv
#        the `build` rung oracles — same drv ⇒ same store path by construction
#        (asserted explicitly), and --check makes the rootless daemon rebuild
#        it and compare bit-for-bit against the root daemon's artifact. On
#        mismatch the divergent rebuild is kept (--keep-failed) and the rung
#        prints the exact diffoscope command to run OUTSIDE the loop
#        (diffoscope is a cold Python closure the offline sandbox cannot
#        build).
#    The recipe does the pinned-guix work ($(GUIX): lower, oracle-build,
#    closure via gc -R); the script does the namespace work with the
#    pin-guarded host guix (time-machine cannot re-resolve channels once
#    /gnu/store is covered). Scratch lives in $(CURDIR)/.rootless-scratch
#    (disk, not the sandbox tmpfs); kept on red for diffing, removed on green.
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
