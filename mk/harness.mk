# mk/harness.mk — the guix-free inner loop, run INSIDE td's OWN /td/store harness
# (host-sandbox-stage0 inc2c). `./check.sh check-harness` enters the harness
# (busybox + GNU make at /td/store, /gnu/store + /var/guix ABSENT, guix off PATH)
# and runs `make -f mk/harness.mk check-harness-inner` with the harness's OWN make.
#
# This file is NOT a gate fragment (it lives in mk/, not mk/gates/, so the Makefile's
# `mk/gates/*.mk` glob never includes it) and may use ONLY the harness userland — no
# guix, no /gnu/store, no host tools.
#
# check.sh passes HBIN (the harness bin dir, /td/store/<set>/bin) and SHELL (the
# harness busybox sh, an absolute path make execs directly). PATH is pinned to HBIN
# so every recipe resolves the busybox applets, never a host binary. The default is
# only a placeholder for `make -f mk/harness.mk` run by hand inside an interactive
# harness; check.sh always overrides HBIN/SHELL on the command line.
HBIN ?= /td/store/bin
export PATH := $(HBIN)

.PHONY: check-harness-inner
check-harness-inner:
	@sh tests/harness-loop.sh
