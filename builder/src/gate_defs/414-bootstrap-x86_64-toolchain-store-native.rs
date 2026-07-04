//! bootstrap-x86_64-toolchain-store-native — source-bootstrap: CROSS the i686 full-source bootstrap UP to a
//! native x86_64 toolchain at /td/store (x86_64-toolchain track). The whole existing /td/store toolchain (gcc
//! 14.3.0 + binutils 2.44 + glibc 2.41) is i686/32-bit (ld-linux.so.2), but the upstream Rust pin is x86_64, so
//! the rust-store-native (#196) runtime leg is blocked on ARCHITECTURE — not just glibc>=2.17. From the 229-byte
//! seed, td builds the i686 chain → gcc 14.3.0, then with it CROSSES UP (LFS/crosstool shape): cross binutils
//! 2.44 (--target=x86_64) → cross gcc 14 stage1 (C, no libc) → MODERN x86_64 glibc 2.41 (ld-linux-x86-64.so.2 +
//! libc.so.6) → cross gcc 14 stage2 (c,c++ --enable-shared → libgcc_s.so.1, which rustc needs). The x86_64 glibc
//! 2.41 is interned content-addressed at /td/store, and the cross gcc links a DYNAMIC x86_64 C AND C++ program
//! against it (interp = /td/store x86_64 ld-linux-x86-64.so.2) that runs in the own-root → 42, /gnu/store ABSENT.
//! DURABLE: pinned-input, no-guix (no /gnu/store in the x86_64 libc.so.6 NOR the cross gcc/cc1), content-addr,
//! repro (gcc14-repro: the cross gcc 14.3.0 is BYTE-REPRODUCIBLE — `--with-as`/`--with-ld`/`--with-sysroot` are
//! pinned to deterministic paths, NOT per-build mktemp, so the gcc driver's .rodata is stable; two independent
//! from-source builds, normalized by tests/repro-lib.sh, land on the SAME bytes → a stable input-addressed
//! artifact; intrinsic double-build, no guix oracle), behavioral (an ELF 64-bit C + C++ program runs vs the
//! x86_64 glibc 2.41 from /td/store → 42), structural,
//! input-addressed (x64-toolchain-subst: the x86_64 glibc is ALSO interned at the LOCK-KEYED path from
//! tests/td-toolchain-x86_64.lock — the stable path a consumer fetches as a signed substitute, not a
//! content-addressed throwaway — and a program whose interp IS that path runs in the own-root → 42), and
//! subst FETCH (x64-toolchain-subst, human 2026-06-28: the loop builds td-subst from source, PUBLISHES that
//! real x86_64 glibc at its lock-keyed path as a SIGNED substitute, then a consumer FETCHES it via
//! tools/resolve-toolchain.sh — ed25519 sig + StorePath == the lock path + NarHash verified — and RUNS the
//! fetched-not-rebuilt bytes in the own-root → 42, with cold-store/wrong-key/wrong-StorePath self-discrimination;
//! DELIBERATE directive-1 relaxation, human-approved). This gate still BUILDS from seed AND fetches — it proves the
//! consumer CAPABILITY (the loop can obtain the real x86_64 toolchain by fetching); the actual per-PR build-SKIP
//! needs the whole-toolchain closure fetch + a populated persistent store and is the PR3b follow-up. The subst round-trip lives in tests/x86_64-subst-lib.sh. NOT a BUILD_GATE. The
//! cross rungs live in tests/x86_64-cross-fns.sh.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "bootstrap-x86_64-toolchain-store-native",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Shared,
        script: r##"
echo ">> bootstrap-x86_64-toolchain-store-native: cross the i686 bootstrap up to a native x86_64 toolchain at /td/store — cross binutils 2.44 + cross gcc 14.3.0 + MODERN x86_64 glibc 2.41 (libgcc_s.so.1); a DYNAMIC x86_64 C AND C++ program runs in the own-root → 42, /gnu/store ABSENT (unblocks the x86_64 Rust runtime leg)"
sh tests/bootstrap-x86_64-toolchain-store-native.sh
"##,
    }
}
