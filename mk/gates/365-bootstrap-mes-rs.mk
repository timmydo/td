# bootstrap-mes-rs — source-bootstrap BRICK 2 as a STRUCTURED Rust recipe
# (rust-migration C2; sibling of gate 361 / C1 affected-checks-rs #226). The
# bootstrap-mes shell driver, ported to a typed Rust `Recipe` (Pin::Source) +
# the shared leg runner in builder/src/bootstrap.rs, run by the stage0 td-builder:
# `td-builder bootstrap-recipe mes`. From the seed, td drives M2-Planet + mescc-tools
# over the td-fetched (pinned, not vendored) GNU Mes 0.27.1 tarball to a working
# mes-m2. Same ALL-DURABLE legs as gate 364 (no guix oracle): pinned-input (the
# warmed tarball == the lock sha256), no-guix (build env-cleared; no /gnu/store in
# mes-m2), behavioral (mes-m2 evaluates Scheme), repro (two builds byte-identical).
# The Rust-built mes-m2 is byte-identical to the shell-built one (own, then diverge).
# Reads the warmed tarball from .td-build-cache/sources (check.sh's HOST prelude
# warms it via tools/warm-bootstrap-sources.sh — the offline loop never egresses).
# The shell tests/bootstrap-mes.sh + gate 364 stay the live driver + removable
# oracle (no cutover this PR). Standalone (~minutes) — NOT a BUILD_GATE.
HEAVY_GATES += bootstrap-mes-rs
bootstrap-mes-rs:
	@echo ">> bootstrap-mes-rs: the structured Rust mes recipe builds brick 2 via the stage0 td-builder — mes-m2 evaluates Scheme, guix-free + reproducible (the recipe-as-data port of gate 364)"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; tb="$$TB"; \
	case "$$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($$tb)" >&2; exit 1 ;; esac; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	"$$tb" bootstrap-recipe mes
