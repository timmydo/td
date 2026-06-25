# bootstrap-gcc-mesboot1 — source-bootstrap BRICK 5: GCC 4.6.4 with C AND C++ (guix's gcc-mesboot1).
# gcc-core-mesboot1 (#176) proved the C build; this overlays the gcc-g++-4.6.4 front-end + builds
# --enable-languages=c,c++ (cc1plus + a static libstdc++) — the c++ compiler the next gcc (gcc-mesboot
# 4.7.4, itself C++) needs. From the 229-byte seed: chain → gcc-mesboot0 → binutils-mesboot1 →
# make-mesboot → gcc-mesboot1 (static; LDFLAGS=-static; MAKEINFO=true; cmp/diff from the store). i686,
# static, serial. DURABLE: pinned-input (chain + 5 boot patches + gcc-4.6.4/gcc-g++/gmp/mpfr/mpc),
# no-guix (no /gnu/store in gcc/g++), behavioral (gcc runs C → 42 AND g++ runs C++ → 42), repro
# (gcc+g++ drivers byte-identical + both emit identical assembly). NOT a BUILD_GATE. gcc-mesboot (4.7.4)
# then the final toolchain are next.
HEAVY_GATES += bootstrap-gcc-mesboot1
bootstrap-gcc-mesboot1:
	@echo ">> bootstrap-gcc-mesboot1: the toolchain builds GCC 4.6.4 with C AND C++ (cc1plus + static libstdc++) — a modern gcc+g++ from the seed, guix-free + reproducible (source-bootstrap brick 5)"
	sh tests/bootstrap-gcc-mesboot1.sh
