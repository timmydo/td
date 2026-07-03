# store-persist — the LOOP builds a corpus package into a PERSISTENT /td/store + DB, and
# a SEPARATE `td-builder` invocation SKIPS the rebuild by reading it back: the incremental
# /td/store, build-into / read-back across builds, wired into the BUILD PATH (not a
# test-only subcommand). Reuses the store-native corpus path (gate 416): from the seed
# `bootstrap_modern_toolchain` builds the /td/store toolchain, then `td-builder
# build-recipe` builds GNU sed 4.9 with it CANONICALLY at /td/store (TD_STORE_DIR) into a
# persistent store P (TD_PERSIST_STORE/TD_PERSIST_DB). Invocation 1 = CACHE=miss +
# build-into (merge_output_db); invocation 2 (fresh scratch) = CACHE=persist — the build
# path finds sed valid in P (persistent_realization) and SKIPS the build; the sed READ BACK
# FROM P runs in the own-root, /gnu/store ABSENT, transforming foo->bar. DURABLE (build-into,
# skip/read-back, behavioral). guix only = the one-time seed capture + the seed toolchain
# (§5, retired last); the build reads the td-owned seed DB, not /var/guix (guix-surface flat).
# Heavy (the /td/store toolchain from the seed); the build-recipes prelude runs → BUILD_GATES.
#
# PARKED — NOT registered in any pool (human direction, PR #291): deterministically RED
# from clean state because build-recipe's staged closure for this gate's invocation
# collapses to the lock's direct entries (no transitive runtime deps — coreutils' gmp is
# dropped, `expr` dies on libgmp.so.10, and sed's configure spins). Full evidence + repro
# in issue #292; the machinery (warm-seed / build-recipe staging) is shared with the
# #287/#288 workstreams. Run it on demand with `./check.sh store-persist`. RE-ENABLE by
# replacing the PARKED_GATES line below with the original registrations when #292 is
# fixed:
#   HEAVY_GATES += store-persist
#   BUILD_GATES += store-persist
PARKED_GATES += store-persist
store-persist:
	@echo ">> store-persist: the loop builds a corpus package at /td/store into a persistent store + DB (build-into), and a SEPARATE invocation SKIPS the rebuild reading it back (CACHE=persist), running it own-root /gnu/store-absent — incremental /td/store, wired into the build path"
	sh tests/store-persist.sh
