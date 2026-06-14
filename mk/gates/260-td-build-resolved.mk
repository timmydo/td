# input-resolution SWAP (DESIGN §7.1 move-off-Guile; "retire input resolution",
# Inc.2). Where `resolve` proves td's lock resolution EQUALS Guile's (additive,
# build unchanged), this is the SWAP: the `td-build` nano build now CONSUMES
# td-builder's lock resolution for its declared deps (ncurses + gettext-minimal)
# instead of Guile's `specification->package`. The deps enter as td-resolved
# input-SOURCES (already-realized store paths from the lock), so NO
# specification->package runs for them; the toolchain stays Guile (retired even
# later, §5). Proven:
#   • SWAP — the nano .drv's input-SOURCES are EXACTLY td-builder's resolved dep
#     paths, and ncurses/gettext are NOT input-derivations (Guile did not resolve
#     the deps);
#   • REPRODUCIBLE — `guix build --check` (verdict-memoized — prime directive 1);
#   • BEHAVIORAL — the td-resolved-deps nano and the corpus nano print byte-
#     identical `--version` (the swap changed the resolution path, not the build),
#     at a DISTINCT store path.
# Verified-red: a perturbed lock makes td resolve a bad dep path → the input-source
# is invalid → the build fails (the build genuinely consumes td's resolution).
# Heavy (a warm nano compile + a --check + the oracle build); next to `resolve`.
HEAVY_GATES += td-build-resolved
td-build-resolved:
	@echo ">> td-build-resolved: the td-build nano build CONSUMES td-builder's lock resolution for its deps (input-sources, no Guile specification->package); reproducible + behaviorally identical to the corpus nano (input-resolution SWAP)"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	echo ">> td-builder RESOLVES the nano recipe's declared deps from the pinned lock (no Guile):"; \
	deps=""; \
	for n in ncurses gettext-minimal; do \
	  p=`"$$tb" resolve "$(CURDIR)/tests/td-build-inputs.lock" "$$n"`; \
	  test -n "$$p" || { echo "FAIL: td-builder resolved no path for '$$n'." >&2; exit 1; }; \
	  echo "   $$n -> $$p"; \
	  deps="$$deps $$p"; \
	done; \
	deps=`echo $$deps | sed 's/^ *//'`; \
	echo ">> lower the nano derivation with the td-resolved deps as input-SOURCES"; \
	vars=`TD_RESOLVED_DEPS="$$deps" $(GUIX) repl $(LOAD) tests/td-build-resolved-drv.scm 2>/dev/null`; \
	td_drv=`printf '%s\n' "$$vars" | sed -n 's/^TD_DRV=//p'`; \
	sources_match=`printf '%s\n' "$$vars" | sed -n 's/^TD_SOURCES_MATCH=//p'`; \
	not_inputdrvs=`printf '%s\n' "$$vars" | sed -n 's/^TD_DEPS_NOT_INPUTDRVS=//p'`; \
	corpus_drv=`printf '%s\n' "$$vars" | sed -n 's/^CORPUS_DRV=//p'`; \
	corpus_out=`printf '%s\n' "$$vars" | sed -n 's/^CORPUS_OUT=//p'`; \
	test -n "$$td_drv" -a -n "$$corpus_drv" -a -n "$$corpus_out" \
	  || { echo "ERROR: could not lower the td-resolved / corpus derivations" >&2; exit 1; }; \
	echo ">> td-resolved nano drv : $$td_drv"; \
	echo ">> SWAP proof: the build's deps came from td's resolution, not Guile"; \
	test "$$sources_match" = "yes" \
	  || { echo "FAIL: the nano .drv's input-sources are NOT exactly td-builder's resolved dep paths — the build did not consume td's resolution." >&2; exit 1; }; \
	test "$$not_inputdrvs" = "yes" \
	  || { echo "FAIL: ncurses/gettext are still input-DERIVATIONS of the nano .drv — Guile's specification->package still resolved the deps (no swap)." >&2; exit 1; }; \
	echo "   deps are td-resolved input-sources; ncurses/gettext are NOT input-derivations"; \
	echo ">> build the td-resolved nano"; \
	out=`$(GUIX) build "$$td_drv"`; \
	test -n "$$out" -a -x "$$out/bin/nano" || { echo "FAIL: the td-resolved build produced no bin/nano" >&2; exit 1; }; \
	echo ">> check: reproducibility (verdict-memoized — prime directive 1)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$td_drv"; \
	echo ">> build the corpus oracle nano (gnu-build-system)"; \
	corpus_out_built=`$(GUIX) build "$$corpus_drv"`; \
	echo ">> behavioral differential: run BOTH, --version must be byte-identical"; \
	td_ver=`"$$out/bin/nano" --version`; \
	corpus_ver=`"$$corpus_out_built/bin/nano" --version`; \
	printf '   td-resolved nano --version: %s\n' "`printf '%s' "$$td_ver" | head -n1`"; \
	printf '   corpus      nano --version: %s\n' "`printf '%s' "$$corpus_ver" | head -n1`"; \
	printf '%s' "$$td_ver" | head -n1 | grep -q "GNU nano, version 8.7.1" || { echo "FAIL: td-resolved nano did not report the expected version." >&2; exit 1; }; \
	test "$$td_ver" = "$$corpus_ver" || { echo "FAIL: td-resolved nano --version differs from the corpus oracle." >&2; exit 1; }; \
	echo ">> independence: the td-resolved artifact is a DISTINCT store object"; \
	test "$$out" != "$$corpus_out" || { echo "FAIL: the td-resolved path equals the corpus path — not an independent build." >&2; exit 1; }; \
	echo "PASS: the td-build nano build consumed td-builder's lock resolution for its deps (input-sources; ncurses/gettext are NOT input-derivations, no specification->package), is reproducible, and prints byte-identical --version to the corpus nano, at a distinct store path ($$out != $$corpus_out)."
