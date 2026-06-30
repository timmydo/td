# td-shell-userland — the REAL `td shell` product command over the REAL shipped Rust
# userland, end-to-end and GUIX-FREE. `td shell ripgrep -- rg PATTERN tree` (and a multi-tool
# `td shell ripgrep fd -- …`) resolves each PKG to a td RECIPE, provisions its crate closure
# GUIX-FREE (intern the warmed source + crate set → build-recipe's TD_VENDOR_DIR form, the
# crate-closure path that until now lived only in the bespoke `crate-free-build.sh` harness),
# builds it with td-builder itself, composes the command's PATH from the td store OUTPUT, and
# execs — with guix/Guile SCRUBBED from PATH, so a green run proves no `guix` process is in the
# resolve/build/exec path. This is the use-case complement to the per-tool `rust-<x>` gates:
# those assert each tool builds == its pin in isolation; THIS asserts a person can actually USE
# the shipped userland through the product command. All legs are DURABLE behavioral (NO guix
# oracle): rg greps a needle (and not the unrelated file); the rg/fd on PATH are td's OWN builds
# at td store paths; fd+rg cooperate in one shell on a real task; an unknown package errors with
# no guix fallback. The rust/gcc toolchain SEED stays guix-built (retired last by the source
# bootstrap). tests/td-shell-userland.sh carries the legs; the ripgrep+fd crate closures are
# warmed by the check.sh prelude (`td-feed warm crate`, sha256 == the crates.io index cksum).
# Build gate (stage0 + td-recipe-eval via the build-recipes prelude) → BUILD_GATES + HEAVY_GATES.
HEAVY_GATES += td-shell-userland
BUILD_GATES += td-shell-userland
td-shell-userland:
	@echo ">> td-shell-userland: td shell builds + runs the real Rust userland (ripgrep, fd) GUIX-FREE and execs a real task (durable behavioral; no guix process, no oracle)"
	sh tests/td-shell-userland.sh
