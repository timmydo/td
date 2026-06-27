# rust-store-native — RELINK the upstream Rust toolchain to /td/store with td's OWN ELF
# rewriter (builder/src/elf.rs `elf-set-interp`, NO patchelf / no guix tool), then intern it
# GUIX-FREE. The bytes are the upstream Rust release tarball (seed/sources/rust-*.lock), not a
# guix build, so this eliminates guix at the source (NOT the demoted store-relocate of a guix
# binary). Toward a usable Rust userspace assembled WITHOUT the guix operating-system
# (system/td.scm) — rust-store-native track. Durable legs: supply-chain (sha==pin),
# provenance (zero /gnu/store), structural (interp -> /td/store, interned content-addressed).
# The /td/store-RUNTIME leg is PENDING the gcc lane's glibc-final (rust needs GLIBC_2.17; the
# /td/store glibc is 2.16.0). See plan/rust-store-native.md.
HEAVY_GATES += rust-store-native
rust-store-native:
	@echo ">> rust-store-native: relink the upstream Rust 1.96.0 toolchain to /td/store (td's own ELF rewriter, no patchelf), intern guix-free; interp -> /td/store, zero /gnu/store; runtime leg pending glibc-final"
	sh tests/rust-store-native.sh
