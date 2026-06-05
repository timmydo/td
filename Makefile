# td — the single pass/fail entry point (CLAUDE.md "The loop").
#
# `make check` runs, in order and short-circuiting on the first failure:
#   1. eval   — load the declaration + test modules (fails fast, sub-second)
#   2. build  — build the bootable image and assert it is reproducible
#   3. test   — boot the marionette system test and assert the kernel release
#
# Every guix invocation is pinned to channels.scm via `guix time-machine`, so
# the reproducibility oracle is honest regardless of the ambient guix version.
# Intended to be run hermetically:  guix shell -C --pure -- make check

GUIX    := guix time-machine -C channels.scm --
LOAD    := -L .
SYSTEM  := system/td.scm
IMGTYPE := qcow2

.PHONY: check eval build test

check: eval build test

# 1. Config eval — load both modules; catches syntax/binding errors in well
#    under a second, before any expensive build.
eval:
	@echo ">> eval: load (system td) and (tests boot)"
	@echo '(begin (use-modules (system td) (tests boot)) (display "eval ok\n"))' \
	  | $(GUIX) repl $(LOAD)

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
