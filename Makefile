# td — the single pass/fail entry point (CLAUDE.md "The loop").
#
# `make check` runs the gate ladder. The authoritative gate list is the set of
# drop-in fragments under mk/gates/*.mk: each fragment registers itself into the
# CHEAP_GATES or HEAVY_GATES pool and carries its own documentation + recipe,
# and the `check:` target expands the two pools. ADDING A GATE: drop a new
# mk/gates/<NNN>-<name>.mk file holding ONE `CHEAP_GATES +=`/`HEAVY_GATES +=`
# line and the recipe — there is no shared list line to edit, so two agents
# adding gates touch two different files and never collide on merge. The numeric
# <NNN> filename prefix sets order (the glob is sorted): cheap gates serial-first
# in NNN order, heavy gates in LPT order for `-j2`. A stale order only costs
# latency, never correctness, so renumbering is a rare, deliberate `git mv`.
#
# Every guix invocation is pinned to channels.scm via `guix time-machine`, so
# the reproducibility oracle is honest regardless of the ambient guix version.
# Run it via `./check.sh` (the hermetic, offline wrapper) — NOT a bare
# `guix shell -C --pure -- make check`, which lacks the store/daemon exposure,
# host-guix-pin guard, and substitute-disabling that keep the loop offline.

# Recipes use bash so multi-command recipes can run under `set -euo pipefail`
# (triage #1): a failure ANYWHERE in a `;`-chained recipe — notably a
# `guix build --check` reproducibility failure or an unreadable artifact — must
# abort the gate, never be swallowed so a later command's success greens it.
SHELL   := bash

GUIX    := guix time-machine -C channels.scm --
LOAD    := -L .
SYSTEM  := system/td.scm
IMGTYPE := qcow2

# Canned lower-then-realise for marionette system tests (the `test`,
# `boot-disk` and `reset` gates; `container` lowers multiple artifacts and
# keeps its own block). Two steps on purpose: `guix repl` reading a script
# from STDIN always exits 0 (it swallows the script's exit code), so building
# the test there would make a FAILED test look green. Instead: (1) lower the
# monadic test value to a derivation file name via repl, then (2) realise it
# with `guix build`, whose exit status is honest and which streams the
# marionette log so failures are visible.
#   $(1) = test module, e.g. (tests boot)
#   $(2) = system-test variable, e.g. %test-td-boot
#   $(3) = label for messages, e.g. boot
define realise-system-test
	@drv=`printf '%s\n' \
	    '(use-modules (guix) (gnu tests) $(1))' \
	    '(with-store store' \
	    '  (set-build-options store #:use-substitutes? #f #:offload? #f)' \
	    '  (format #t "DRV=~a~%"' \
	    '          (derivation-file-name' \
	    '           (run-with-store store (system-test-value $(2))))))' \
	  | $(GUIX) repl $(LOAD) 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the $(3) test derivation" >&2; exit 1; }; \
	echo ">> realise $(3) test derivation: $$drv"; \
	$(GUIX) build "$$drv"
endef

# Bare `make` runs the in-sandbox loop, never the sandbox wrapper — guards
# against `container-check` (which calls ./check.sh) being the default goal and
# recursing into nested containers.
.DEFAULT_GOAL := check

# Assemble the gate pools from the drop-in fragments. Each mk/gates/*.mk does
# `CHEAP_GATES += <name>` or `HEAVY_GATES += <name>` (plus `FAST_GATES += <name>`
# for the fast tier) right next to its recipe; sorting the glob makes the numeric
# filename prefixes the authoritative order. .PHONY, the `check` targets, the
# serial chain, and the heavy gate are all DERIVED from these pools, so the lists
# cannot drift apart.
#
# CHEAP_GATES are the sub-5s structural gates; their NNN order IS their strict
# serial execution order (a generated order-only chain below), so a syntax error
# or differential regression reds the loop before any VM boots or tarball repacks.
#
# HEAVY_GATES run at most two at a time under check.sh's `make -j2` (DESIGN §7.3:
# more concurrent VMs may thrash; empirically the daemon overlaps two client
# builds). They are ordered LONGEST-FIRST (LPT packing) by their NNN prefix:
# under -j2 make starts them in list order, and seeding the slots with the
# longest gates lets the short ones fill the gaps instead of leaving a long gate
# to run alone at the end. RE-MEASURE AND RE-SORT (renumber the fragments)
# whenever the full-check wall time drifts; per-gate cost notes live in the
# fragment headers and plan/loop-latency.md. A stale order only costs latency.
#
# NOTHING is removed, loosened, or skipped by the parallelism: all gates must
# still pass, and make (run without -k) stops spawning new gates after a failure
# — a red still short-circuits the loop. Order-only (|) prerequisites, so a plain
# serial `make -j1 check` behaves exactly as before.
CHEAP_GATES :=
HEAVY_GATES :=
FAST_GATES  :=
include $(sort $(wildcard mk/gates/*.mk))

.PHONY: check check-fast container-check list-gates $(CHEAP_GATES) $(HEAVY_GATES)

# The hermetic, offline, self-contained entry point (DESIGN §1.1/§1.4). Plain
# `make check` assumes you are ALREADY inside the right `guix shell -C` sandbox;
# `make container-check` (or ./check.sh) sets that sandbox up for you. Prefer it.
container-check:
	@./check.sh

check: $(CHEAP_GATES) $(HEAVY_GATES)

# The fast tier — the gates that test td's OWN surface (typed/TS front-end + the
# Rust builder/evaluator) and need only the toolchain: no `guix system image`,
# no marionette VM, no QEMU/kernel/bootloader closure. A STRICT SUBSET of
# `check` (FAST_GATES are tagged inside the relevant heavy fragments), for quick
# "is td's logic right" feedback and for a light CI job that need not import the
# full system/boot closure. PURELY ADDITIVE: `check` above is unchanged and
# remains the gate; nothing here removes, loosens, reorders, or skips a gate.
check-fast: $(CHEAP_GATES) $(FAST_GATES)

# Print the assembled gate pools — the one-screen overview the single-file list
# used to give. (`make list-gates`, no build.)
list-gates:
	@echo "cheap ($(words $(CHEAP_GATES))): $(CHEAP_GATES)"
	@echo "heavy ($(words $(HEAVY_GATES))): $(HEAVY_GATES)"
	@echo "fast  ($(words $(FAST_GATES))): $(FAST_GATES)"

# Generated ordering graph (do not hand-edit): chain each cheap gate order-only
# on its predecessor, and gate every heavy gate on the last cheap gate.
chain-prev :=
$(foreach r,$(CHEAP_GATES),$(eval $(if $(chain-prev),$(r): | $(chain-prev)))$(eval chain-prev := $(r)))
$(HEAVY_GATES): | $(lastword $(CHEAP_GATES))

# `rootless` runs LAST, alone, IN A FULL CHECK. It snapshots the LIVE store DB
# (mk/gates/130 → tests/rootless.sh: copy + wal_checkpoint); a CONCURRENTLY-
# building gate would leave an active WAL the non-root snapshot cannot read.
# Gating it order-only on every OTHER heavy gate makes make start it only once
# they have all finished, so it snapshots a QUIESCENT DB. A scheduling constraint
# within td's sandbox (td is the sole loop container — there is no guix-shell-C
# carve-out), not a gate-list edit; cost is rootless's wall time serial at the
# tail (plan/loop-sandbox.md R8).
#
# But ONLY when rootless is part of a larger goal. When it is the SOLE explicit
# goal (`make rootless` / `./check.sh rootless`) the order-only prereqs would drag
# in the whole heavy ladder, breaking the single-target contract (CLAUDE.md
# "./check.sh <target> runs a single Makefile target"). Suppress the ordering in
# that case so an explicit `rootless` runs alone — it is already quiescent when
# nothing else is building.
ifneq ($(MAKECMDGOALS),rootless)
rootless: | $(filter-out rootless,$(HEAVY_GATES))
endif
