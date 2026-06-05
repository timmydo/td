# td — the single pass/fail entry point (CLAUDE.md "The loop").
#
# `make check` runs, in order and short-circuiting on the first failure:
#   1. eval     — load the declaration + test modules (fails fast, sub-second)
#   2. diff     — typed front-end lowers to the same SYSTEM drv as the gexp (M4)
#   3. oci-diff — typed front-end lowers to the same OCI image drv as the gexp (M5)
#   4. build    — build the bootable image and assert it is reproducible
#   5. test     — boot the marionette system test and assert the kernel release
#   6. oci      — build the Docker/OCI image and assert it is reproducible (M5)
#
# Every guix invocation is pinned to channels.scm via `guix time-machine`, so
# the reproducibility oracle is honest regardless of the ambient guix version.
# Intended to be run hermetically:  guix shell -C --pure -- make check

GUIX    := guix time-machine -C channels.scm --
LOAD    := -L .
SYSTEM  := system/td.scm
IMGTYPE := qcow2

# Bare `make` runs the in-sandbox loop, never the sandbox wrapper — guards
# against `container-check` (which calls ./check.sh) being the default goal and
# recursing into nested containers.
.DEFAULT_GOAL := check

.PHONY: check container-check eval diff oci-diff manifest-diff build test oci

# The hermetic, offline, self-contained entry point (DESIGN §1.1/§1.4). Plain
# `make check` assumes you are ALREADY inside the right `guix shell -C` sandbox;
# `make container-check` (or ./check.sh) sets that sandbox up for you. Prefer it.
container-check:
	@./check.sh

check: eval diff oci-diff manifest-diff build test oci

# 1. Config eval — load both modules; catches syntax/binding errors in well
#    under a second, before any expensive build.
eval:
	@echo ">> eval: load (system td), (system td-typed) and (tests boot)"
	@echo '(begin (use-modules (system td) (system td-typed) (tests boot)) (display "eval ok\n"))' \
	  | $(GUIX) repl $(LOAD)

# M4 differential (DESIGN §2.4/§2.5). Cheap structural check — lowers systems to
# derivations, no building — so it runs right after eval and fails fast. Run as
# a repl SCRIPT (not piped via STDIN) so the script's `(exit)` is the rung's
# exit status; a piped script would always exit 0 and hide a red (see `test`).
diff:
	@echo ">> diff: typed front-end lowers to the same store path as the gexp"
	$(GUIX) repl $(LOAD) tests/typed-diff.scm

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
	    '  (format #t "DRV=~a~%"' \
	    '          (derivation-file-name' \
	    '           (run-with-store store (system-test-value %test-td-boot)))))' \
	  | $(GUIX) repl $(LOAD) 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the test derivation" >&2; exit 1; }; \
	echo ">> realise test derivation: $$drv"; \
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
