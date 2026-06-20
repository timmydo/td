section: side
status: claimed
title: cmake-build-system
handle: claude-opus-fcfa37
date: 2026-06-19
notes: plan/cmake-build-system.md
summary: Add a cmake build system to td-builder's own Rust builder so td builds a cmake-based package from source with NO gnu-build-system and NO guix/Guile in the build path. build-recipe routes buildSystem "cmake" to a new cmake-build phase runner (set-paths -> unpack -> cmake configure -> make -> make install), modelled on the autotools (gnu) path. Smallest increment: a trivial in-tree CMakeLists demonstrator (td-cmake-demo, builds a hello-style binary), interned at gate time, proven end to end by a new gate (mk/gates/350-cmake.mk) with guix/Guile scrubbed from PATH: STRUCTURAL (built guix/guile off PATH), DURABLE behavioral (the artifact runs), DURABLE repro (td-builder check double-build), and the removable migration-oracle leg (distinct store path from guix's cmake-build-system). cmake is a guix-built SEED input (toolchain retired last, DESIGN §5).
