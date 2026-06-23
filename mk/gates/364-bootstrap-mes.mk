# bootstrap-mes — source-bootstrap BRICK 2 (north star: no guix BYTES). From the 229-byte stage0
# seed, td builds M2-Planet + mescc-tools (brick 1) and drives them over the GNU Mes RELEASE
# SOURCE — the pinned mes-0.27.1.tar.gz (seed/sources/mes-*.lock), td-fetched (not vendored, not
# guix-fetched) in check.sh's prelude into .td-build-cache/sources/ — to compile + link a working
# GNU Mes Scheme interpreter, mes-m2 — guix-free. ALL-DURABLE:
#   [pinned-input] the warmed tarball matches the lock sha256 — built from the exact pinned bytes;
#   [no-guix]    the whole chain runs with guix/Guile off env; no /gnu/store byte in mes-m2;
#   [behavioral] the seed-built mes-m2 evaluates Scheme (display + arithmetic) from the Mes
#     module tree — a real interpreter, not just a linked ELF;
#   [repro]      two independent mes builds yield a byte-identical mes-m2.
# Standalone (static seed tools + ~seconds of M2-Planet/M1/hex2) — NOT a BUILD_GATE, never pulls
# build-recipes. Brick 3 bootstraps tinycc from mes; bricks 4-5 reach gcc/glibc at /td/store.
HEAVY_GATES += bootstrap-mes
bootstrap-mes:
	@echo ">> bootstrap-mes: from the seed, td builds GNU Mes (mes-m2) and proves it evaluates Scheme, guix-free + reproducible (source-bootstrap brick 2)"
	sh tests/bootstrap-mes.sh
