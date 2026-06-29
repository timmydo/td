# 9. offline-isolation sandbox probe (S1). The
#    hermeticity clause says an UNDECLARED fetch — network access from a
#    non-fixed-output builder — must be impossible; until now that was an
#    assumed property of guix-daemon's sandbox, never an asserted gate. This
#    realises tests/offline-drv.scm's DRV_SANDBOX probe: a regular derivation
#    whose builder must see ONLY `lo` in /proc/net/dev and whose TCP egress
#    attempt must raise — i.e. the deliberate undeclared fetch demonstrably
#    fails. Then `guix build --check` re-runs the builder, so the assertions
#    RE-EXECUTE every loop (and the probe is proven reproducible, prime
#    directive 1) — a daemon regression (e.g. --disable-chroot) reds this gate
#    on the next check, not just on a cold store. Self-discriminating across
#    contexts: check.sh's host-side control proves the SAME /proc/net/dev
#    mechanism reports non-lo interfaces where network IS present, and the
#    fixed-output twin (DRV_DAEMON, wired in at S2) is the same builder body
#    failing red in a network-visible netns (verified-red evidence).
#    Cheapest heavy gate (one tiny local build) →
#    listed last (LPT).
HEAVY_GATES += offline
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
