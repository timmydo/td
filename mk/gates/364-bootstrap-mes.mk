# bootstrap-mes — source-bootstrap BRICK 2 (north star: no guix BYTES). From the 229-byte stage0
# seed, td builds M2-Planet + mescc-tools (brick 1) and drives them over the vendored GNU Mes
# source (seed/mes, pinned mes f244b141 / 0.27.1) to compile + link a working GNU Mes Scheme
# interpreter, mes-m2 — guix-free. ALL-DURABLE:
#   [no-guix]    the whole chain runs with guix/Guile off env; no /gnu/store byte in mes-m2;
#   [behavioral] the seed-built mes-m2 evaluates Scheme (display + arithmetic) from its own
#     vendored module tree — a real interpreter, not just a linked ELF;
#   [repro]      two independent mes builds yield a byte-identical mes-m2.
# Standalone (static seed tools + ~seconds of M2-Planet/M1/hex2) — NOT a BUILD_GATE, never pulls
# build-recipes. Brick 3 bootstraps tinycc from mes; bricks 4-5 reach gcc/glibc at /td/store.
HEAVY_GATES += bootstrap-mes
bootstrap-mes:
	@echo ">> bootstrap-mes: from the seed, td builds GNU Mes (mes-m2) and proves it evaluates Scheme, guix-free + reproducible (source-bootstrap brick 2)"
	sh tests/bootstrap-mes.sh
