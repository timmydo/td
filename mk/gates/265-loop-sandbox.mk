# loop-sandbox (DESIGN §7.1; gate-2 "Loop tooling convergence"). Toward replacing
# `guix shell -C` with td's OWN sandbox: `td-builder host-sandbox` is a DEV-SHELL (vs.
# the build jail) — it pivots into a fresh root exposing ONLY the WHOLE /gnu/store
# (read-only), the daemon socket /var/guix, /proc and /dev, with host-guix on PATH and
# the host filesystem otherwise GONE. This gate is the gate-2 OBSERVE step done
# additively (it does NOT touch check.sh's real `guix shell -C` entry — directive 3):
# (1) EXPOSURE EQUIVALENCE — plain `guix build -d hello` lowers to the SAME .drv path
# inside td's host-sandbox as it does directly under check.sh's `guix shell -C` (guix's
# container is the oracle, directive 4); equal path proves td's sandbox exposes the
# store + daemon socket + guix the same way. (2) ISOLATION — the host worktree
# ($(CURDIR)/Makefile, visible in `guix shell -C`'s shared cwd) is INVISIBLE inside
# td's sandbox, while /gnu/store + the socket remain exposed — proving it is a real
# container, not a bare userns. (3) NET-NAMESPACE PARITY — td's sandbox enters its OWN
# network namespace (its /proc/self/ns/net inode differs from the gate's), loopback-only
# (no host interface) with `lo` brought up, matching `guix shell -C`'s offline posture;
# the daemon stays reachable across it (the Unix socket on the bound /var/guix), proven
# by the exposure equivalence holding. Scope (honest, deferred follow-up like the build
# jail deferred NEWPID/chroot to S4): the wholesale check.sh swap is the remaining LATER
# increment; this gate still runs INSIDE check.sh's offline outer container. Heavy (a
# td-builder compile + a few nested-sandbox guix/bash probes), in the heavy pool.
HEAVY_GATES += loop-sandbox
loop-sandbox:
	@echo ">> loop-sandbox: td's OWN sandbox hosts a loop step (guix build -d) byte-identical to guix shell -C, and isolates the host filesystem"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	echo ">> exposure equivalence: plain 'guix build -d hello' under guix shell -C vs inside td's host-sandbox"; \
	oracle=`guix build -d hello`; \
	test -n "$$oracle" || { echo "ERROR: oracle 'guix build -d hello' produced nothing" >&2; exit 1; }; \
	tdout=`"$$tb" host-sandbox -- guix build -d hello`; \
	echo "   guix shell -C  : $$oracle"; \
	echo "   td host-sandbox: $$tdout"; \
	test "$$tdout" = "$$oracle" \
	  || { echo "FAIL: td's sandbox lowered a DIFFERENT .drv than guix shell -C ($$tdout vs $$oracle) — exposure diverged" >&2; exit 1; }; \
	echo ">> isolation: the host worktree is invisible inside td's sandbox, while the store + socket stay exposed"; \
	realbash=`readlink -f "$$(command -v bash)"`; \
	if "$$tb" host-sandbox -- "$$realbash" -c "test -e '$(CURDIR)/Makefile'"; then \
	  echo "FAIL: the host worktree ($(CURDIR)) leaked into td's sandbox — not isolated" >&2; exit 1; fi; \
	"$$tb" host-sandbox -- "$$realbash" -c "test -d /gnu/store && test -S /var/guix/daemon-socket/socket" \
	  || { echo "FAIL: td's sandbox did not expose /gnu/store + the daemon socket" >&2; exit 1; }; \
	echo "   worktree gone; /gnu/store + daemon socket exposed"; \
	echo ">> net-namespace parity: td's sandbox runs in its OWN netns (like guix shell -C), loopback-only, daemon reachable across it"; \
	realreadlink=`readlink -f "$$(command -v readlink)"`; \
	parent_ns=`readlink /proc/self/ns/net`; \
	td_ns=`"$$tb" host-sandbox -- "$$realbash" -c 'exec "$$0" /proc/self/ns/net' "$$realreadlink"`; \
	echo "   guix shell -C netns: $$parent_ns"; \
	echo "   td host-sandbox netns: $$td_ns"; \
	case "$$td_ns" in net:\[*\]) : ;; *) echo "FAIL: td's sandbox netns '$$td_ns' is not a net namespace link" >&2; exit 1;; esac; \
	test "$$td_ns" != "$$parent_ns" \
	  || { echo "FAIL: td's sandbox did not enter its OWN netns (same as guix shell -C's $$parent_ns) — no net isolation" >&2; exit 1; }; \
	"$$tb" host-sandbox -- "$$realbash" -c 'ifaces=""; while IFS= read -r l; do case "$$l" in *:*) n="$${l%%:*}"; ifaces="$$ifaces $${n// /}";; esac; done < /proc/net/dev; test "$$ifaces" = " lo"' \
	  || { echo "FAIL: td's sandbox netns is not loopback-only (a non-lo interface is present)" >&2; exit 1; }; \
	echo "   td entered its own loopback-only netns; the daemon stayed reachable (the equivalence above held across it)"; \
	echo "PASS: td's OWN sandbox (td-builder host-sandbox) hosted 'guix build -d hello' to the SAME .drv as check.sh's guix shell -C (store + daemon socket + guix exposed identically) while ISOLATING the host filesystem (worktree gone) AND running in its OWN loopback-only network namespace (net parity with guix shell -C; the daemon stays reachable over the Unix socket); the gate-2 OBSERVE step — check.sh's entry is unchanged, the wholesale swap is the remaining follow-up."
