# bootstrap-seed — source-bootstrap BRICK 0 (the north star: no guix BYTES). td's /td/store
# toolchain is built up from a tiny, hand-auditable, NON-guix seed — stage0-posix's 229-byte
# hex0-seed + 618-byte kaem-optional-seed (vendored in seed/stage0/, pinned to stage0-posix-x86
# 3b9c2bb). This gate runs the seed kaem build with guix/Guile SCRUBBED from env, producing the
# first stage0 artifacts (a full hex0 + kaem-0) — no guix process, no /gnu/store in the build.
# ALL-DURABLE (the seed is the irreducible bottom; there is no guix oracle):
#   [no-guix] the vendored seeds match their pinned sha256 (auditable, not guix-built);
#   [self-reproduction] the seed assembles its OWN hex source to a byte-identical seed (so the
#     binary seeds are verifiable from the human-readable hex, not blind trust);
#   [behavioral] the seed-built hex0 actually works as an assembler (reproduces kaem-0);
#   [repro] two independent runs are byte-identical.
# Standalone + tiny (two ~hundred-byte assemblers, sub-second) — NOT a BUILD_GATE, so it never
# pulls build-recipes. Later bricks drive kaem-0 over the rest of the chain (mes→tinycc→gcc→glibc).
HEAVY_GATES += bootstrap-seed
bootstrap-seed:
	@echo ">> bootstrap-seed: the 229-byte auditable hex0-seed (NOT guix-built) builds the first stage0 artifacts with guix off env — self-reproducing, working, reproducible (source-bootstrap brick 0)"
	sh tests/bootstrap-seed.sh
