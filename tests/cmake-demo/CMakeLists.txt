# tests/cmake-demo — a minimal cmake C project, used by the `cmake` gate to prove
# td-builder builds a cmake-based package from source with NO gnu-build-system and
# NO guix/Guile in the build path (move-off-Guile §5). td's own cmake phase runner
# (build::run_cmake) configures this out-of-source, builds it, and installs the
# `td-cmake-hello` binary into $out/bin.
cmake_minimum_required(VERSION 3.10)
project(td-cmake-demo C)

# A fixed-output binary so the gate's DURABLE behavioral leg can assert its output.
add_executable(td-cmake-hello hello.c)

# install -> $out/bin/td-cmake-hello (CMAKE_INSTALL_PREFIX = $out, set by run_cmake).
install(TARGETS td-cmake-hello DESTINATION bin)
