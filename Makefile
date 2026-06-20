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
SYSTEM_GATES :=
# BUILD_SPECS — every package recipe the parallel `build-recipes` phase realizes +
# reproducibility-checks up front (the corpus, toolchain leaves and library deps). Each
# package-build gate fragment appends its OWN spec list (so the list lives next to the
# gate that asserts on it — no drift, no shared line to collide on). BUILD_GATES — the
# gates that consume that warm cache; they get an order-only dep on `build-recipes`.
BUILD_SPECS :=
BUILD_GATES :=
include $(sort $(wildcard mk/gates/*.mk))

.PHONY: check check-fast check-system container-check list-gates build-recipes $(CHEAP_GATES) $(HEAVY_GATES) $(SYSTEM_GATES)

# The hermetic, offline, self-contained entry point (DESIGN §1.1/§1.4). Plain
# `make check` assumes you are ALREADY inside the right `guix shell -C` sandbox;
# `make container-check` (or ./check.sh) sets that sandbox up for you. Prefer it.
container-check:
	@./check.sh

check: $(CHEAP_GATES) build-recipes $(HEAVY_GATES)

# build-recipes — the PARALLEL build phase (DESIGN §7.1 move-off-Guile §5). Separates
# "build everything" from "the checks": realize + reproducibility-check EVERY package
# recipe ($(BUILD_SPECS) — the corpus, toolchain leaves and library deps) up front,
# fanned out across cores, into the shared content-addressed cache (.td-build-cache/pkg).
# The package build gates then cache-HIT the build and memo-skip the repro double-build,
# so they only run their durable behavioral + migration-oracle assertions. Each build is
# single-threaded (the builder runs make serially, NIX_BUILD_CORES=1), so the fan-out is
# ~nproc wide with no internal oversubscription — overridable with TD_BUILD_JOBS. This is
# the heavy lifting that USED to run serial-within-gate under -j2; the cache makes the
# gates' re-build a no-op, so NOTHING is weakened — the same .drv is assembled, realized
# and double-built, just once and in parallel. Listed first among `check`'s heavy
# prerequisites (after the cheap serial chain) so make starts it right after the
# fail-fast structural gates; the build gates wait on it via the order-only dep below.
build-recipes:
	@echo ">> build-recipes: realize + reproducibility-check $(words $(BUILD_SPECS)) recipes in parallel into .td-build-cache/pkg ($(BUILD_SPECS))"
	@set -euo pipefail; \
	node=`$(GUIX) build node`/bin/node; \
	tsc=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-typescript)'`; \
	seedev=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-ts-eval)'`/bin/td-ts-eval; \
	test -x "$$seedev" -a -x "$$node" -a -n "$$tsc" || { echo "ERROR: could not resolve node / tsc / td-ts-eval seed" >&2; exit 1; }; \
	for s in $(BUILD_SPECS); do grep ' /gnu/store/' "tests/$$s-no-guix.lock"; done \
	  | sed 's/^[^ ]* //' | sort -u | xargs $(GUIX) build >/dev/null \
	  || { echo "ERROR: could not realize the build seed (regenerate locks on a channel bump)" >&2; exit 1; }; \
	grep ' /gnu/store/' tests/td-builder-rust.lock | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null \
	  || { echo "ERROR: could not realize the stage0 toolchain seed (regenerate tests/td-builder-rust.lock on a channel bump)" >&2; exit 1; }; \
	grep ' /gnu/store/' tests/td-ts-eval.lock | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null \
	  || { echo "ERROR: could not realize the td-ts-eval seed + crates (regenerate tests/td-ts-eval.lock on a boa bump)" >&2; exit 1; }; \
	export TD_NODE="$$node" TD_TSC="$$tsc" TD_TSDIR="$(CURDIR)/tests/ts"; \
	export CACHE="$(CURDIR)/.td-build-cache/pkg" TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; mkdir -p "$$CACHE"; \
	. tests/cache-lib.sh; load_stage0; \
	echo ">> builds run on the td-bootstrapped stage0 td-builder ($$TD_BUILDER_PATH) — NO guix-built td-builder (move-off-Guile §5 brick 3)"; \
	tdeval=`TD_TS_EVAL="$$seedev" sh tests/ts-eval-tool.sh "$(CURDIR)/.td-build-cache/rust-ts-eval"`; \
	test -x "$$tdeval" || { echo "ERROR: could not build the td-ts-eval evaluator (brick 4b prelude)" >&2; exit 1; }; \
	export TD_TS_EVAL="$$tdeval"; \
	echo ">> recipes EVALUATE with td's OWN td-ts-eval ($$tdeval) — not the guix-built one (move-off-Guile §5 brick 4b)"; \
	jobs=$${TD_BUILD_JOBS:-$$(nproc)}; \
	echo ">> building $(words $(BUILD_SPECS)) recipes across $$jobs cores (single-threaded each) ..."; \
	printf '%s\n' $(BUILD_SPECS) | xargs -P "$$jobs" -n1 sh tests/build-pkg.sh; \
	echo "PASS: build-recipes — all $(words $(BUILD_SPECS)) package recipes realized + reproducible in the shared cache (.td-build-cache/pkg); the build gates now cache-hit + memo-skip the double-build and only assert behavior/oracle."

# The fast tier — the gates that test td's OWN surface (typed/TS front-end + the
# Rust builder/evaluator) and need only the toolchain: no `guix system image`,
# no marionette VM, no QEMU/kernel/bootloader closure. A STRICT SUBSET of
# `check` (FAST_GATES are tagged inside the relevant heavy fragments), for quick
# "is td's logic right" feedback and for a light CI job that need not import the
# full system/boot closure. PURELY ADDITIVE: `check` above is unchanged and
# remains the gate; nothing here removes, loosens, reorders, or skips a gate.
check-fast: $(CHEAP_GATES) $(FAST_GATES)

# The `guix system` / whole-OS tier — gates that boot a VM (QEMU/marionette/
# SSH-harness), build/realize a full system or OCI *image*, or diff the system
# image / generation roots. DELIBERATELY PARKED out of the default `check` while
# td's focus is the user-space PACKAGE MANAGER (build/realize/store/recipes) — not
# `guix system`. This is a human-directed scope decision (DESIGN §4.3 / directive
# 3): the gates are NOT deleted or weakened, only moved to an on-demand tier; the
# package-manager loop (`check`) keeps its OWN behavioral coverage (built tools
# run, link-tests, the guix per-PACKAGE differential). Re-fold these back into
# `check` when the OS becomes the focus. Run them on demand: `./check.sh check-system`.
check-system: $(CHEAP_GATES) $(SYSTEM_GATES)

# Print the assembled gate pools — the one-screen overview the single-file list
# used to give. (`make list-gates`, no build.)
list-gates:
	@echo "cheap  ($(words $(CHEAP_GATES))): $(CHEAP_GATES)"
	@echo "heavy  ($(words $(HEAVY_GATES))): $(HEAVY_GATES)"
	@echo "fast   ($(words $(FAST_GATES))): $(FAST_GATES)"
	@echo "system ($(words $(SYSTEM_GATES))): $(SYSTEM_GATES)"

# Generated ordering graph (do not hand-edit): chain each cheap gate order-only
# on its predecessor, and gate every heavy gate on the last cheap gate.
chain-prev :=
$(foreach r,$(CHEAP_GATES),$(eval $(if $(chain-prev),$(r): | $(chain-prev)))$(eval chain-prev := $(r)))
$(HEAVY_GATES): | $(lastword $(CHEAP_GATES))
# System-tier gates (on-demand `check-system`) gate on the last cheap gate too, so
# the structural gates run serial-first there exactly as in `check`.
$(SYSTEM_GATES): | $(lastword $(CHEAP_GATES))
# The parallel build phase runs after the fail-fast cheap gates; the package build
# gates (BUILD_GATES) then wait on it, so by the time they run the cache is warm — they
# cache-hit + memo-skip and only assert behavior/oracle. The dep is on the build gates
# (not all heavy gates), so the light tiers stay light: `check-fast` (cheap + ts) and
# `check-system` never trigger build-recipes.
build-recipes: | $(lastword $(CHEAP_GATES))
$(BUILD_GATES): | build-recipes

# `rootless` needs NO special scheduling — it runs as an ordinary heavy gate
# under -j2 (its only prereq is the generic last-cheap-gate one above). It USED to
# be serialized alone at the tail because it SNAPSHOTTED the live store DB by
# copying /var/guix/db: a concurrently-building gate's active WAL could tear the
# non-root copy. Since the rootless-snapshot-race fix (PR #53) the gate instead
# CONSTRUCTS its snapshot DB from the static closure (`td-builder store-register`
# — mk/gates/130-rootless.mk / tests/rootless.sh, sealed against any live-DB
# read), so the cross-check race is eliminated BY CONSTRUCTION, not mitigated by
# ordering. Dropping the old `rootless: | $(filter-out rootless,$(HEAVY_GATES))`
# constraint removes dead-weight tail latency (rootless is the single longest
# heavy gate, so running it alone last was doubly wasteful) and weakens no gate:
# rootless's validity guard, the daemon-oracle differential, and the live-DB-read
# seal all still run and must pass.
