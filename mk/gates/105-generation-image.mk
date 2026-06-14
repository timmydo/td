# M10.1 bootc generation image (M10-design.md "What a generation bundle is").
# td's OCI lowering emits userspace ONLY; this builds the bootc-style image that
# makes it bootable by APPENDING a /boot layer (kernel + initrd) — see
# (system td-generation). Heavier gate (builds a system docker image + repacks),
# so it runs with the other image gates. Validator scratch lives in
# $(CURDIR)/.genimg-scratch — disk, not the sandbox /tmp (the oci-load lesson:
# that tmpfs is a small RAM fraction and the extraction is multi-GB; it
# ENOSPC'd on a 16G-RAM CI host); kept on red for triage, removed on green.
# Self-discriminating at the artifact
# level (like manifest-check/no-guix), and it --checks reproducibility (prime
# directive 1 — this IS the new artifact):
#   • build the gen-1 + gen-2 bootc images, and `--check` BOTH bit-for-bit;
#   • crack each image's layers and assert /boot/bzImage AND /boot/initrd.cpio.gz
#     are PRESENT in the bootc image and ABSENT from the plain userspace image
#     (DRV_BASE) — the discriminator for the "made bootable" claim;
#   • assert gen-1 and gen-2 lower to DIFFERENT image derivations — each carries
#     its own generation's initrd (which mounts that generation's distinct root),
#     so the bundle is genuinely per-generation, not a shared artifact.
HEAVY_GATES += generation-image
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
	echo ">> check: reproducibility of BOTH bootc generation images (verdict-memoized — tests/check-memo.sh; a miss runs the real --check unchanged)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$gen1" "$$gen2"; \
	echo ">> validate artifacts (structured: guile-json metadata + guile-zlib initrd)"; \
	scratch="$(CURDIR)/.genimg-scratch"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	TD_GEN1_IMG="$$gen1_img" TD_GEN2_IMG="$$gen2_img" TD_BASE_IMG="$$base_img" \
	  TMPDIR="$$scratch" $(GUIX) repl $(LOAD) tests/generation-image-check.scm; \
	rm -rf "$$scratch"
