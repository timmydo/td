# M10.1 per-generation root (DESIGN §2.3 generations; M10-design.md P1). Cheap,
# derivation/record-level, self-discriminating like the diffs above: prove the
# typed `generation` field derives a DISTINCT, bootloader-selectable root per
# generation — (a) generation #f still converges to the shared-root oracle, (b)
# two generations get different root labels AND different system drvs, (c) a
# generation's root is not the shared td-root. Without this each generation would
# boot the same filesystem and rollback would be a no-op. The full boot+rollback
# is M10.3. Run as a repl SCRIPT so `(exit)` is the gate's status.
SYSTEM_GATES += generation-diff
generation-diff:
	@echo ">> generation-diff: each generation gets a distinct, selectable root (M10.1)"
	$(GUIX) repl $(LOAD) tests/generation-diff.scm
