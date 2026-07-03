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

# --- Per-gate wall-clock instrumentation (task L1) -------
# Every TIMED gate (see the .SHELLFLAGS block after the gate pools) runs its
# recipe through tools/gate-time.sh, which logs a START/END event per recipe
# line to a per-run log; tools/gate-timing-report.sh reduces each gate's
# min(START)/max(END) into a wall-clock span so latency regressions are visible
# and the heavy-gate LPT order is renumbered from DATA, not the hand-run numbers.
# The log is integer-nanosecond timestamps (the sandbox
# has no awk). One log per make invocation (TD_GATE_RUN keeps concurrent runs in
# the same worktree from clobbering each other); the report keeps the newest 10.
TD_GATE_TIMING_DIR := $(CURDIR)/.td-build-cache/gate-timing
TD_GATE_RUN := $(shell date +%s%N)
export TD_GATE_TIMING_LOG := $(TD_GATE_TIMING_DIR)/run-$(TD_GATE_RUN).log

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
# fragment headers. A stale order only costs latency.
#
# NOTHING is removed, loosened, or skipped by the parallelism: all gates must
# still pass, and make (run without -k) stops spawning new gates after a failure
# — a red still short-circuits the loop. Order-only (|) prerequisites, so a plain
# serial `make -j1 check` behaves exactly as before.
CHEAP_GATES :=
HEAVY_GATES :=
FAST_GATES  :=
SYSTEM_GATES :=
ENGINE_GATES :=
# PARKED_GATES — gates a human has UNHOOKED from every check tier pending a tracked
# fix (the gate file documents the issue + how to re-enable). No tier consumes this
# pool; members stay runnable on demand (`./check.sh <gate>`).
PARKED_GATES :=
# BUILD_SPECS — every package recipe the parallel `build-recipes` phase realizes +
# reproducibility-checks up front (the corpus, toolchain leaves and library deps). Each
# package-build gate fragment appends its OWN spec list (so the list lives next to the
# gate that asserts on it — no drift, no shared line to collide on). BUILD_GATES — the
# gates that consume that warm cache; they get an order-only dep on `build-recipes`.
BUILD_SPECS :=
BUILD_GATES :=
include $(sort $(wildcard mk/gates/*.mk))

.PHONY: check check-fast check-system check-engine container-check list-gates build-recipes gate-timing-report $(CHEAP_GATES) $(HEAVY_GATES) $(SYSTEM_GATES)

# The hermetic, offline, self-contained entry point (DESIGN §1.1/§1.4). Plain
# `make check` assumes you are ALREADY inside the right `guix shell -C` sandbox;
# `make container-check` (or ./check.sh) sets that sandbox up for you. Prefer it.
container-check:
	@./check.sh

check: $(CHEAP_GATES) build-recipes $(HEAVY_GATES)
	@TD_HEAVY_GATES='$(HEAVY_GATES)' sh tools/gate-timing-report.sh "$(TD_GATE_TIMING_DIR)" "$(TD_GATE_TIMING_DIR)/latest.txt" || true

# build-recipes — the build phase (DESIGN §7.1 move-off-Guile §5). Separates "build
# everything" from "the checks": td-ASSEMBLE + SUBMIT every package recipe ($(BUILD_SPECS) —
# the corpus, toolchain leaves and library deps) up front to the ONE shared build daemon,
# which realizes + reproducibility-checks them into the shared content-addressed store, then
# the package build gates cache-HIT + memo-skip the double-build and only assert behavior +
# migration-oracle. The daemon (tools/build-daemon-ensure.sh, started by check.sh's host
# prelude) is the SINGLE machine-wide build limiter: it caps concurrent builds at ONE global
# budget shared by ALL agents/worktrees, so N concurrent checks can no longer oversubscribe
# the box or OOM it. The `-P` below is now only SUBMIT parallelism (submits block on the
# daemon's budget) — it no longer sets build concurrency; the daemon does (TD_BUILD_JOBS).
# Listed first among `check`'s heavy prerequisites so make starts it right after the
# fail-fast structural gates; the build gates wait on it via the order-only dep below.
build-recipes:
	@echo ">> build-recipes: assemble + submit $(words $(BUILD_SPECS)) recipes to the shared build daemon (global budget), then reproducibility-check ($(BUILD_SPECS))"
	@set -euo pipefail; \
	: "$${TD_DAEMON_SOCKET:?the shared build daemon is not running — check.sh starts it in its host prelude (tools/build-daemon-ensure.sh)}"; \
	for s in $(BUILD_SPECS); do grep ' /gnu/store/' "tests/$$s-no-guix.lock"; done \
	  | sed 's/^[^ ]* //' | sort -u | xargs $(GUIX) build >/dev/null \
	  || { echo "ERROR: could not realize the build seed (regenerate locks on a channel bump)" >&2; exit 1; }; \
	grep ' /gnu/store/' tests/td-builder-rust.lock | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null \
	  || { echo "ERROR: could not realize the stage0 toolchain seed (regenerate tests/td-builder-rust.lock on a channel bump)" >&2; exit 1; }; \
	export CACHE="$(CURDIR)/.td-build-cache/pkg" TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; mkdir -p "$$CACHE"; \
	. tests/cache-lib.sh; load_stage0; \
	echo ">> builds run on the td-bootstrapped stage0 td-builder ($$TD_BUILDER_PATH) — NO guix-built td-builder (move-off-Guile §5 brick 3)"; \
	TD_GUIX="$(GUIX)" sh tests/recipe-eval-tool.sh "$(CURDIR)/.td-build-cache/recipe-eval" >/dev/null \
	  || { echo "ERROR: could not build td's Rust recipe evaluator (recipes/ crate)" >&2; exit 1; }; \
	load_recipe_eval; \
	echo ">> recipes EVALUATE with td's OWN Rust td-recipe-eval ($$TD_RECIPE_EVAL) — boa retired (rust-recipe-surface)"; \
	echo ">> submitting $(words $(BUILD_SPECS)) recipes to the shared build daemon ($$TD_DAEMON_SOCKET); the daemon's global budget caps concurrency ..."; \
	printf '%s\n' $(BUILD_SPECS) | xargs -P "$(words $(BUILD_SPECS))" -n1 sh tests/build-pkg.sh; \
	echo "PASS: build-recipes — all $(words $(BUILD_SPECS)) package recipes realized + reproducible via the shared build daemon into .td-build-cache/pkg; the build gates now cache-hit + memo-skip the double-build and only assert behavior/oracle."

# The fast tier — the gates that test td's OWN surface (typed/TS front-end + the
# Rust builder/evaluator) and need only the toolchain: no `guix system image`,
# no marionette VM, no QEMU/kernel/bootloader closure. A STRICT SUBSET of
# `check` (FAST_GATES are tagged inside the relevant heavy fragments), for quick
# "is td's logic right" feedback and for a light CI job that need not import the
# full system/boot closure. PURELY ADDITIVE: `check` above is unchanged and
# remains the gate; nothing here removes, loosens, reorders, or skips a gate.
check-fast: $(CHEAP_GATES) $(FAST_GATES)

# The IMAGE tier — the gates that ship + run td-native OCI images (oci-native,
# rust-userland-image). The old guix-system museum that filled this pool (guix
# qcow2/docker images, generations, signed registry, placement, rootless-daemon
# differentials) was RETIRED wholesale (human direction, directive 3:
# "I never wanted the guix museum" — the guix operating-system was scaffolding,
# not the product; td ships td-native images of td-built packages, and the
# retained gates test exactly that). Run on demand: `./check.sh check-system`;
# the daily backstop runs it too.
check-system: $(CHEAP_GATES) $(SYSTEM_GATES)
	@TD_HEAVY_GATES='$(SYSTEM_GATES)' sh tools/gate-timing-report.sh "$(TD_GATE_TIMING_DIR)" "$(TD_GATE_TIMING_DIR)/latest.txt" || true

# The build-ENGINE smoke tier — a TRUE smoke: "does it compile, lint, and pass unit tests",
# targeting ~2 min so a build-engine change (builder/src/*) lands fast WITHOUT the full
# corpus. It is the cheap structural gates + `cargo-test` (compile the engine + run its
# drv/store/NAR/scan/sandbox unit tests) — and NOTHING that builds a package from source.
# Anything heavier (bootstrap-build/build-plan/td-check/corpus/repro/system) is NOT smoke;
# it stays in the full `check`, run DAILY by the agent-driven backstop (DESIGN §7.2, human
# 2026-06-21) — no longer a per-PR gate. (`lint`, the structural shell/convention checks,
# runs in CI on every PR.) PURELY ADDITIVE: `check` above is unchanged. Run it:
# `./check.sh check-engine`.
check-engine: $(CHEAP_GATES) $(ENGINE_GATES)

# Print the assembled gate pools — the one-screen overview the single-file list
# used to give. (`make list-gates`, no build.)
list-gates:
	@echo "cheap  ($(words $(CHEAP_GATES))): $(CHEAP_GATES)"
	@echo "heavy  ($(words $(HEAVY_GATES))): $(HEAVY_GATES)"
	@echo "fast   ($(words $(FAST_GATES))): $(FAST_GATES)"
	@echo "system ($(words $(SYSTEM_GATES))): $(SYSTEM_GATES)"
	@echo "engine ($(words $(ENGINE_GATES))): $(ENGINE_GATES)"
	@echo "parked ($(words $(PARKED_GATES))): $(PARKED_GATES)"

# Generated ordering graph (do not hand-edit): chain each cheap gate order-only
# on its predecessor, and gate every heavy gate on the last cheap gate.
chain-prev :=
$(foreach r,$(CHEAP_GATES),$(eval $(if $(chain-prev),$(r): | $(chain-prev)))$(eval chain-prev := $(r)))
$(HEAVY_GATES): | $(lastword $(CHEAP_GATES))
# System-tier gates (on-demand `check-system`) gate on the last cheap gate too, so
# the structural gates run serial-first there exactly as in `check`.
$(SYSTEM_GATES): | $(lastword $(CHEAP_GATES))
# Engine-smoke gates (on-demand `check-engine`) gate on the last cheap gate too. They are
# all STANDALONE (not BUILD_GATES), so `check-engine` never pulls in `build-recipes`.
$(ENGINE_GATES): | $(lastword $(CHEAP_GATES))
# The parallel build phase runs after the fail-fast cheap gates; the package build
# gates (BUILD_GATES) then wait on it, so by the time they run the cache is warm — they
# cache-hit + memo-skip and only assert behavior/oracle. The dep is on the build gates
# (not all heavy gates), so the light tiers stay light: `check-fast` (cheap + ts) and
# `check-system` never trigger build-recipes.
build-recipes: | $(lastword $(CHEAP_GATES))
$(BUILD_GATES): | build-recipes

# --- Per-gate wall-clock instrumentation wiring (task L1) ----------------------
# Route every TIMED gate's recipe through tools/gate-time.sh by overriding ONLY
# its `.SHELLFLAGS` (SHELL stays plain bash): make then invokes each recipe line
# as `bash tools/gate-time.sh <gate> -c '<recipe>'`. A non-default .SHELLFLAGS
# also disables make's direct-exec fast path, so every line is wrapped. This is
# scoped strictly to the gate targets — `check`, helper, and report targets keep
# the default shell. Fail-safe and opt-out: it engages only when the wrapper
# file exists (`$(wildcard)`) and TD_GATE_TIMING is not 0, and the wrapper itself
# never alters a gate's exit status (see tools/gate-time.sh). The build phase
# (build-recipes) is timed too — it is the loop's single largest cost.
TIMED_GATES := $(CHEAP_GATES) $(HEAVY_GATES) $(SYSTEM_GATES) $(ENGINE_GATES) build-recipes
ifneq ($(wildcard tools/gate-time.sh),)
ifneq ($(TD_GATE_TIMING),0)
$(TIMED_GATES): .SHELLFLAGS = $(CURDIR)/tools/gate-time.sh $@ -c
endif
endif

# Standalone report (`make gate-timing-report`): re-print the most recent run's
# per-gate table for when a gate failed (so `check`'s end-of-run report did not
# fire) or to inspect a `check-system`/`check-fast` run. `check` runs it
# automatically on a green loop (its recipe above).
gate-timing-report:
	@TD_HEAVY_GATES='$(HEAVY_GATES)' sh tools/gate-timing-report.sh "$(TD_GATE_TIMING_DIR)" "$(TD_GATE_TIMING_DIR)/latest.txt"

