# td-check (DESIGN §7.1; gate-2 of the move-off-Guile arc — td OWNS the reproducibility
# oracle). Prime directive 1 says *reproducibility is a test*; today that verdict is
# `guix build --check` (the daemon builds twice and compares). This gate has td compute
# that verdict ITSELF: `td-builder check` executes the `td-build` hello `.drv` TWICE in
# two INDEPENDENT user-namespace sandbox runs (reusing the #25 executor) and compares
# the per-output NAR hashes (reusing the #21/S2 NAR serializer + SHA-256) — equal ⇒
# reproducible, with no daemon and no `guix build --check` in td's verdict. The gate
# then proves that verdict matches guix's: td's reproducible NAR hash equals the
# daemon's RECORDED hash, AND the differential oracle `guix build --check` agrees the
# SAME `.drv` is reproducible (prime directive 4 — proven equal before any later
# replacement; nothing existing is loosened, directive 3). Honest scope: input
# resolution + the closure (`guix gc -R`) + the daemon building the INPUTS stay Guix's;
# only the TOP derivation's reproducibility is td's double-build (toolchain retired
# last, §5). Heavy (a td-builder compile + a daemon hello build for the oracle + TWO td
# hello builds + a --check), so it slots in the heavy pool by the other td gates.
# Scratch on disk (two staged build trees), kept on red for triage, removed on green.
HEAVY_GATES += td-check
ENGINE_GATES += td-check   # build-engine smoke (check-engine): reproducibility double-build oracle
td-check:
	@echo ">> td-check: td computes the reproducibility verdict ITSELF — builds the td-build hello .drv TWICE (independent userns sandboxes), NAR-equal, matching guix build --check"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; tb="$$TB"; \
	case "$$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($$tb)" >&2; exit 1 ;; esac; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-check-scratch"; chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	$(GUIX) repl $(LOAD) tests/td-drv-build-drv.scm 2>/dev/null > "$$scratch/facts.txt"; \
	drv=`sed -n 's/^HELLO_DRV=//p' "$$scratch/facts.txt"`; \
	out=`sed -n 's/^HELLO_OUT=//p' "$$scratch/facts.txt"`; \
	hash=`sed -n 's/^HELLO_HASH=//p' "$$scratch/facts.txt"`; \
	test -n "$$drv" -a -n "$$out" -a -n "$$hash" \
	  || { echo "ERROR: could not lower/daemon-build the hello drv oracle facts" >&2; exit 1; }; \
	echo ">> subject .drv: $$drv"; \
	echo ">> oracle (daemon-recorded): $$out  nar-hash sha256:$$hash"; \
	echo ">> stage the input closure"; \
	{ sed -n 's/^HELLO_INPUT=//p' "$$scratch/facts.txt"; echo "$$drv"; } | xargs $(GUIX) gc -R | sort -u > "$$scratch/paths.txt"; \
	echo "   staged closure: $$(wc -l < "$$scratch/paths.txt") store items"; \
	echo ">> td's OWN reproducibility verdict: build the .drv TWICE (two independent userns sandboxes) and compare per-output NAR hashes"; \
	"$$tb" check "$$drv" "$$scratch/paths.txt" "$$scratch/c" > "$$scratch/checkout.txt" 2>"$$scratch/check.err" \
	  || { echo "FAIL: td-builder check reported NON-reproducible (or errored):" >&2; cat "$$scratch/checkout.txt" "$$scratch/check.err" >&2; exit 1; }; \
	grep -qx "CHECK out $$out sha256:$$hash reproducible" "$$scratch/checkout.txt" \
	  || { echo "FAIL: td-builder check did not report 'out $$out sha256:$$hash reproducible' — its two builds disagree or differ from the daemon's recorded hash:" >&2; cat "$$scratch/checkout.txt" >&2; exit 1; }; \
	echo "   td double-build agrees: $$out is reproducible, sha256:$$hash (== the daemon's recorded hash)"; \
	echo ">> differential ORACLE: guix build --check agrees the SAME .drv is reproducible"; \
	$(GUIX) build --check "$$drv" >/dev/null 2>&1 \
	  || { echo "FAIL: guix build --check disagreed — the oracle says the .drv is NON-reproducible" >&2; exit 1; }; \
	echo "   guix build --check agrees: reproducible"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; \
	echo "PASS: td-builder computed the reproducibility verdict ITSELF — built the td-build hello .drv twice in independent userns sandboxes to a byte-identical NAR (== the daemon's recorded hash) — matching guix build --check on the same .drv; no daemon and no guix build --check in td's verdict, which are only the oracle (input resolution + the daemon building the inputs stay Guix's, §5)."
