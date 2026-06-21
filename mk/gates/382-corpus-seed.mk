# corpus-seed — North-Star: ONE warmed seed builds MULTIPLE corpus packages with no guix
# install. seed-build proved hello builds from the seed; this generalizes it — a single
# warmed seed (the union of the packages' build closures) builds two DIFFERENT corpus tools
# (hello + which) from source, each with the seed DB as its ONLY store DB (/var/guix + the
# live /gnu/store out of every build, every input staged from the seed). Proves the seed
# mechanism scales to the corpus: one seed, many builds, no guix install. Leaf recipes use
# build-recipe's seed-store override (#133) — no code change. tests/corpus-seed.sh; guix/Guile
# scrubbed from the build PATH. Heavy (stage0 + a shared seed + two source builds) →
# BUILD_GATES + HEAVY_GATES. Chained corpus (build-plan seed support) is the next step.
HEAVY_GATES += corpus-seed
BUILD_GATES += corpus-seed
corpus-seed:
	@echo ">> corpus-seed: one warmed seed builds two different corpus packages (hello + which) from source, no guix install (the seed scales to the corpus)"
	sh tests/corpus-seed.sh
