# userland-x86_64-store-native — host-sandbox-stage0 inc2 (NO GUIX BYTES): the guix-less
# daily-suite captured set's C userland — busybox 1.37.0 + GNU make 4.4.1 — built FROM
# upstream source (td-fetch, sha-pinned) by the from-seed /td/store x86_64 toolchain (reused
# from the x86_64 gate as a function library), DYNAMIC vs the /td/store glibc 2.41 (interp =
# /td/store/ld), interned at /td/store, and RUN in the store-ns own-root with /gnu/store
# ABSENT. busybox = a POSIX userland (surfaces GNUisms); make = the explicit build driver.
# Durable supply-chain/provenance/no-guix/structural/behavioral legs; verified-red in-gate
# (without the interp relink the own-root run fails). HEAVY (~90 min from seed; directive 1 —
# no cache). NOT a BUILD_GATE. (td-builder, the engine, joins the set via rust-store-native
# rung 3; this proves the busybox+make half.)
HEAVY_GATES += userland-x86_64-store-native
userland-x86_64-store-native:
	@echo ">> userland-x86_64-store-native: busybox + GNU make built from upstream source by the /td/store toolchain, dynamic vs /td/store glibc, run in the store-ns own-root — NO /gnu/store, no guix bytes"
	sh tests/userland-x86_64-store-native.sh
