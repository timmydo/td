# bootstrap-glibc-241-store-native — source-bootstrap BRICK 6/7 (the FINAL modern toolchain, rung C): a MODERN
# glibc 2.41 (guix's glibc-final) at the dynamic /td/store — completing the full modern toolchain from the
# 229-byte seed. td builds the chain → GCC 4.9.4 → MODERN GCC 14.3.0 + a sandbox-runnable MODERN binutils 2.44,
# then with them builds MODERN glibc 2.41 (a SHARED libc). 2.41 is interned content-addressed at /td/store, and
# gcc 14.3.0 links a DYNAMIC C AND C++ (libstdc++ <vector>) program against the NEW glibc 2.41 (interp =
# /td/store glibc 2.41) that runs in the own-root → 42, /gnu/store ABSENT. The full modern toolchain (gcc
# 14.3.0 + binutils 2.44 + glibc 2.41) now lives at /td/store, all from the seed — unblocks the Rust userland
# (needs glibc >= 2.17) and the retirement of the guix toolchain seed. glibc-2.41-specific: needs modern
# binutils 2.44 (2.20.1a too old), `gawk` by name, and forbids DT_RPATH/DT_RUNPATH in libc.so.6 (bake no
# -rpath; LD_LIBRARY_PATH for the build tools). BYTE-REPRODUCIBLE: two independent from-source builds,
# canonicalized by tests/repro-lib.sh (strip the build-path-bearing DWARF + deterministic archives + drop
# libtool .la), land on the SAME content-addressed /td/store path — a stable key for td-subst. DURABLE:
# pinned-input, no-guix (no /gnu/store in libc.so.6 NOR gcc/cc1), content-addr, repro (intrinsic double-build,
# no guix oracle), behavioral (C + C++ vs glibc 2.41 → 42 from /td/store), structural. NOT a BUILD_GATE.
HEAVY_GATES += bootstrap-glibc-241-store-native
bootstrap-glibc-241-store-native:
	@echo ">> bootstrap-glibc-241-store-native: a MODERN glibc 2.41 at /td/store — built from the seed by gcc 14.3.0 + binutils 2.44; gcc 14.3.0 links a DYNAMIC C AND C++ program against it that runs → 42, /gnu/store ABSENT (source-bootstrap brick 6/7, final-toolchain rung C — the full modern toolchain)"
	sh tests/bootstrap-glibc-241-store-native.sh
