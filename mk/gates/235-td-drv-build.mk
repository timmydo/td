# td-drv-build (DESIGN §7.1; the capstone of the §5 move-off-Guile arc). Stitches
# #22 (emit) + #21 (the autotools-build builder) + td-builder S3/S4 (the executor):
# for the `td-build` hello subject, td-builder EMITS the `.drv` AND EXECUTES it in its
# own user-namespace sandbox, output NAR-equal to the daemon's build of the same
# recipe — so construct AND execute are td's Rust, the derivation's builder is
# `td-builder autotools-build` run by `td-builder build`, with NO guile in either; the
# daemon is ONLY the oracle (prime directive 4). The gate: lower + daemon-build the
# hello drv (the oracle facts via query-path-info); `drv-emit-to` writes the emitted
# `.drv` (asserted byte-identical to guix's); stage the input closure; `td-builder
# build` the EMITTED file; assert the registered output — path, NAR hash, size,
# deriver — equals the daemon's recorded facts. Reuses the td-builder S3/S4 harness.
# Scope boundary (honest): input resolution + the closure (`guix gc -R`) + the daemon
# BUILDING the inputs stay Guix's; only the TOP derivation is td-constructed +
# td-executed (toolchain retired last, §5). Heavy (a td-builder compile + two hello
# compiles — daemon oracle + td executor), so it slots in the heavy pool by
# td-builder; RE-MEASURE and RE-SORT once it has run. Scratch on disk (the staged
# closure + output), kept on red for triage, removed on green.
HEAVY_GATES += td-drv-build
td-drv-build:
	@echo ">> td-drv-build: td-builder EMITS the td-build hello .drv AND EXECUTES it (userns sandbox), output NAR-equal to the daemon — no guile in construct or execute"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; tb="$$TB"; \
	case "$$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($$tb)" >&2; exit 1 ;; esac; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-drv-build-scratch"; chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	$(GUIX) repl $(LOAD) tests/td-drv-build-drv.scm 2>/dev/null > "$$scratch/facts.txt"; \
	drv=`sed -n 's/^HELLO_DRV=//p' "$$scratch/facts.txt"`; \
	out=`sed -n 's/^HELLO_OUT=//p' "$$scratch/facts.txt"`; \
	hash=`sed -n 's/^HELLO_HASH=//p' "$$scratch/facts.txt"`; \
	narsize=`sed -n 's/^HELLO_NARSIZE=//p' "$$scratch/facts.txt"`; \
	deriver=`sed -n 's/^HELLO_DERIVER=//p' "$$scratch/facts.txt"`; \
	test -n "$$drv" -a -n "$$out" -a -n "$$hash" -a -n "$$narsize" -a -n "$$deriver" \
	  || { echo "ERROR: could not lower/daemon-build the hello drv oracle facts" >&2; exit 1; }; \
	echo ">> oracle (daemon-built): $$out  nar-hash sha256:$$hash"; \
	echo ">> td-builder EMITS the .drv (construct, #22) — byte-identical to guix's:"; \
	"$$tb" drv-emit "$$drv" >/dev/null \
	  || { echo "FAIL: td's construction is not byte-identical to guix's .drv" >&2; exit 1; }; \
	computed=`"$$tb" drv-emit-to "$$drv" "$$scratch/emitted.drv" 2>"$$scratch/emit.err"` \
	  || { echo "FAIL: td-builder drv-emit-to failed:" >&2; cat "$$scratch/emit.err" >&2; exit 1; }; \
	test "$$computed" = "$$drv" \
	  || { echo "FAIL: td computed a different .drv store path ($$computed vs $$drv)" >&2; exit 1; }; \
	echo "   emitted .drv byte-identical to guix's (drv-emit verified), at $$computed"; \
	echo ">> stage the input closure"; \
	{ sed -n 's/^HELLO_INPUT=//p' "$$scratch/facts.txt"; echo "$$drv"; } | xargs $(GUIX) gc -R | sort -u > "$$scratch/paths.txt"; \
	echo "   staged closure: $$(wc -l < "$$scratch/paths.txt") store items"; \
	echo ">> td-builder EXECUTES the EMITTED .drv in its userns sandbox (builder=td-builder autotools-build, #21):"; \
	"$$tb" build "$$scratch/emitted.drv" "$$scratch/paths.txt" "$$scratch/b" > "$$scratch/buildout.txt" \
	  || { echo "FAIL: td-builder could not build the emitted hello .drv" >&2; cat "$$scratch/buildout.txt" >&2; exit 1; }; \
	grep -qx "OUT=out $$out" "$$scratch/buildout.txt" \
	  || { echo "FAIL: td-builder built a different output than the daemon's $$out" >&2; exit 1; }; \
	reg="$$scratch/b/registration"; \
	test -s "$$reg" || { echo "FAIL: td-builder wrote no registration record" >&2; exit 1; }; \
	echo ">> differential: td's registered output vs the daemon's recorded facts"; \
	grep -qx "path $$out" "$$reg" || { echo "FAIL: store-path mismatch (record below) vs $$out" >&2; cat "$$reg" >&2; exit 1; }; \
	grep -qx "nar-hash sha256:$$hash" "$$reg" || { echo "FAIL: NAR hash mismatch — td '$$(sed -n 's/^nar-hash //p' "$$reg")' vs daemon 'sha256:$$hash'" >&2; exit 1; }; \
	grep -qx "nar-size $$narsize" "$$reg" || { echo "FAIL: NAR size mismatch — td '$$(sed -n 's/^nar-size //p' "$$reg")' vs daemon '$$narsize'" >&2; exit 1; }; \
	grep -qx "deriver $$deriver" "$$reg" || { echo "FAIL: deriver mismatch — td '$$(sed -n 's/^deriver //p' "$$reg")' vs daemon '$$deriver'" >&2; exit 1; }; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; \
	echo "PASS: td-builder EMITTED the td-build hello .drv (byte-identical to guix's) AND EXECUTED it in its own userns sandbox, registering the daemon's exact facts (path, NAR hash, size, deriver) — no guile in construct or execute; the daemon is only the oracle."
