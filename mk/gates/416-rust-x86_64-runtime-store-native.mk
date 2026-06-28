# rust-x86_64-runtime-store-native — rust-store-native track: the /td/store-RUNTIME leg #196 left
# [PENDING glibc-final]. From the 229-byte seed, td builds the i686 chain → gcc 14.3.0, CROSSES UP to
# a native x86_64 toolchain (REUSING the #201 rungs — the x86_64 gate is sourced as a function library,
# not copied), builds x86_64 zlib from source, RELINKS the upstream Rust 1.96.0 rustc + cargo to
# /td/store (td's own ELF rewriter, no patchelf) with their full runtime closure co-located, and RUNS
# rustc -vV + cargo --version from /td/store in the store-ns own-root → rustc/cargo 1.96.0, /gnu/store
# ABSENT. An x86_64 Rust toolchain that runs with no guix process AND no guix bytes in its store.
# DURABLE: supply-chain (sha==pin), provenance (no /gnu/store upstream), no-guix (interned package
# /gnu/store-free), structural (interp ∈ /td/store; complete lib closure), behavioral (it RUNS).
# HEAVY (~90 min from the seed; directive 1 — no cache). NOT a BUILD_GATE. The cross rungs live in
# tests/x86_64-cross-fns.sh; the i686 base + x86_64 cross rungs are reused from the x86_64 gate.
HEAVY_GATES += rust-x86_64-runtime-store-native
rust-x86_64-runtime-store-native:
	@echo ">> rust-x86_64-runtime-store-native: relink the upstream x86_64 Rust 1.96.0 toolchain to /td/store and RUN rustc -vV + cargo --version from the store-ns own-root → rustc/cargo 1.96.0, /gnu/store ABSENT (the rust-store-native runtime leg, on the from-seed x86_64 /td/store toolchain + a from-source x86_64 zlib)"
	sh tests/rust-x86_64-runtime-store-native.sh
