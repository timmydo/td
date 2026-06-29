# bootstrap-seed-rs — source-bootstrap BRICK 0 as a STRUCTURED Rust recipe
# (rust-migration C2, plan/rust-migration.md "C. Scripts -> Rust"; sibling of C1
# affected-checks-rs #226). The bootstrap-seed shell driver, ported to typed Rust
# data + the shared leg runner in builder/src/bootstrap.rs and run by the stage0
# td-builder: `td-builder bootstrap-recipe seed`. Same ALL-DURABLE legs as gate 360
# (no guix oracle — the seed IS the irreducible bottom): pinned-input (vendored
# seeds == pins, no /gnu/store), no-guix (build env-cleared; no /gnu/store in the
# artifacts), self-reproduction (the seed assembles its own hex source byte-identical),
# behavioral (the built hex0 works as an assembler), repro (two builds byte-identical).
# The shell tests/bootstrap-seed.sh + gate 360 stay the live driver + removable
# differential oracle (no cutover this PR). Standalone + tiny (sub-second after the
# stage0 td-builder build) — NOT a BUILD_GATE.
HEAVY_GATES += bootstrap-seed-rs
bootstrap-seed-rs:
	@echo ">> bootstrap-seed-rs: the structured Rust seed recipe builds brick 0 via the stage0 td-builder — self-reproducing, working, reproducible, guix-free (source-bootstrap brick 0, the recipe-as-data port of gate 360)"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; tb="$$TB"; \
	case "$$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($$tb)" >&2; exit 1 ;; esac; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	"$$tb" bootstrap-recipe seed
