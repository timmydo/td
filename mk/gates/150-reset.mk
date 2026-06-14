# 3c. Ephemerality of the CoW reset (loop-latency; DESIGN §1.5). Boots the SAME
#     instrumented qcow2 derivation as boot-disk (cache hit, no extra image
#     build) three times on explicit qcow2 overlays: dirt written on overlay A,
#     dirt STILL THERE on reused overlay A (negative control — writes really
#     persist without a reset), dirt GONE on fresh overlay B (the reset). Makes
#     the loop's fresh-state-per-test guarantee an assertion instead of an
#     implicit property of qemu flags, so any future cycle-time change that
#     leaks guest state across boots goes red here. Same honest two-step
#     lower-then-realise as `test`/`boot-disk`.
HEAVY_GATES += reset
reset:
	@echo ">> reset: CoW overlay reset discards dirtied guest state (ephemerality)"
	$(call realise-system-test,(tests reset),%test-td-reset,reset)
