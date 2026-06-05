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

# 3. Boot + behavioral — run the marionette test derivation; its builder exits
#    non-zero if the assertion (uname -r == declared kernel release) fails, so a
#    failed test makes `build-derivations` raise and this rung go red.
#    Driven through `guix repl` because the test value is a monadic value that
#    must be run against the store (guix build -e cannot lower it directly).
test:
	@echo ">> test: boot marionette + assert kernel release"
	@printf '%s\n' \
	  '(use-modules (guix) (gnu tests) (tests boot))' \
	  '(with-store store' \
	  '  (let ((drv (run-with-store store (system-test-value %test-td-boot))))' \
	  '    (build-derivations store (list drv))' \
	  '    (format #t "test ok: ~a~%" (derivation-file-name drv))))' \
	  | $(GUIX) repl $(LOAD)
