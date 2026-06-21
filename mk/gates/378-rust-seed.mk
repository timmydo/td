# rust-seed — RUST IN THE SEED (North-Star, human 2026-06-21): td builds its own Rust
# BUILD ENGINE (td-builder) from a FROZEN SEED carrying the rust toolchain, no guix
# install in the build path. The Rust analog of the seed-build gate (376, which built
# hello/C from a seed): it captures the rust toolchain closure (tests/td-builder-rust.lock
# roots + the stage0 builder's runtime refs) into a tarball, `seed-unpack`s it into a fresh
# td store, and `build-recipe`s td-builder (recipe-td-builder.ts, buildSystem rust) with the
# unpacked seed as its store DB (TD_SEED_STORE/TD_SEED_DB) — so /var/guix + the live
# /gnu/store toolchain are out of the build's input path. Proves the seed mechanism extends
# to the toolchain td can't self-build ("it takes rust to build rust"). Composes existing
# primitives (build-seed-tarball.sh + seed-unpack + recipe-td-builder.ts #84) — no builder
# change. guix/Guile scrubbed from PATH; guix is only the one-time capture SOURCE + the
# removable oracle. Durable structural/behavioral/repro legs + the removable guix-seed
# differential. Heavy (stage0 + capture + a self-host build + a double-build check), and a
# BUILD_GATE so it slots after the parallel build-recipes fan-out (its cargo build would
# otherwise contend for cores).
HEAVY_GATES += rust-seed
BUILD_GATES += rust-seed
rust-seed:
	@echo ">> rust-seed: td builds td-builder (its Rust engine) from a FROZEN seed that carries the rust toolchain — /var/guix + live /gnu/store toolchain out of the build path; it runs, agrees with guix's, is reproducible (RUST IN THE SEED, North-Star)"
	sh tests/rust-seed.sh
