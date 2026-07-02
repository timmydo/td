# td-shell-userland — the end-to-end gate for the shipped Rust userland, driven through the REAL
# `td shell <tool> -- <tool> <real-task>` PRODUCT command over td's OWN /td/store toolchain,
# GUIX-FREE (no guix rust, no guix gcc-toolchain in the build path — the #258 "347/371 cutover").
# A person types `td shell ripgrep -- rg PATTERN tree`; the shipped tool builds on demand with td's
# native x86_64 gcc 14.3.0 + binutils 2.44 + glibc 2.41 (assembled by gate 416/424's library,
# fetched from the signed subst store or built from seed), links the /td/store glibc (ELF interp =
# /td/store/ld), and RUNS in a store-ns own-root with /gnu/store ABSENT. run_shell learned the
# native path (builder/src/main.rs run_shell_native): it stages the built tool into the native
# store and execs it in the own-root binding /td/store + the cwd — so `td shell` is the guix-free
# product command, not a bespoke harness. Per tool: [behavioral] the tool does its real job in the
# own-root; [no-guix] the built tool references no guix rust/gcc-toolchain, interp = /td/store/ld;
# [native-arch] the linker was the native x86_64 gcc (ELF64); [td-built] the bin is td's own build
# staged in the /td/store store; [supply-chain] every vendored crate's sha256 ∈ the shipped
# Cargo.lock; [repro] the double-build is reproducible (prime directive 1). The structural legs are
# SUPPORTING evidence behind the behavioral run, never the point (AGENTS.md). Crate closures are
# warmed by the check.sh prelude (`td-feed warm crate`); the rust tarball + coreutils/bash/tar/gzip
# build seed are the retired-last seed. The shipped userland set (TD_USERLAND_TOOLS) grows tool by
# tool as each is verified through the product command; ripgrep is the template. HEAVY (assembles
# the native toolchain + warms td-recipe-eval itself), so NOT a BUILD_GATE — like gate 424.
HEAVY_GATES += td-shell-userland
td-shell-userland:
	@echo ">> td-shell-userland: td shell builds + runs the shipped Rust userland over td's OWN /td/store toolchain (native gcc/binutils/glibc; guix rust + gcc-toolchain removed), guix-free own-root run"
	@GUIX="$(GUIX)" ROOT="$(CURDIR)" sh tests/td-shell-userland.sh
