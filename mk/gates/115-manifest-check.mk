# 5. M6 manifest-swap reproducibility — build a SWAPPED-manifest OCI image
#    generation (default manifest + GNU hello) and `--check` it bit-for-bit.
#    `manifest-diff` proves a changed manifest is a DIFFERENT image; this proves
#    that swapped generation is itself reproducible (DESIGN §6 image-swap-only;
#    prime directive 1 — a non-reproducible swapped image is a FAILING test).
#    The less-frequent/heavier gate (§1.3): it repacks a second docker tarball,
#    but hello's closure is tiny and the OS closure is shared with `oci`, so it
#    stays in budget. Two-step (lower to a drv via repl, then realise+check via
#    `guix build`) for the same honest-exit-status reason as the `test` gate.
#    It ALSO inspects the realized tarball (triage #5): manifest-diff only proves
#    the package is in the declaration (operating-system-packages) — an exporter
#    bug could change the image derivation yet omit the files. So here we crack
#    open the built layer.tar and assert hello/bin/hello is actually present in
#    the SWAPPED image and ABSENT from the default image — artifact contents, not
#    just the declaration.
HEAVY_GATES += manifest-check
manifest-check:
	@echo ">> manifest-check: build a SWAPPED-manifest OCI image and --check it"
	@set -euo pipefail; \
	drv=`$(GUIX) repl $(LOAD) tests/manifest-image-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the swapped OCI image derivation" >&2; exit 1; }; \
	echo ">> swapped OCI image derivation: $$drv"; \
	swapped_img=`$(GUIX) build "$$drv"`; \
	echo ">> check: reproducibility of the SWAPPED OCI image derivation (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$drv"; \
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
