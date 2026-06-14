# td-builder S1 toolchain probe + S2 NAR differential (DESIGN §7.1 side-track;
# plan/td-builder.md). The growing gate of the first Guix-component replacement
# (§2.5 discipline) — each sub-task adds a leg, none is ever removed:
#   • S1: lower the td-builder package to a drv (tests/td-builder-drv.scm),
#     build it offline, `guix build --check` it bit-for-bit (prime directive 1;
#     --check re-runs the compile, so a toolchain regression reds the loop),
#     RUN the binary and assert its sentinel (the toolchain produced a WORKING
#     executable — stronger than "cargo build exited 0"), and record closure
#     size + compile wall-clock (§1.3). The crate's unit tests (FIPS SHA-256
#     vectors, NAR framing/sort) also run inside the build (#:tests? #t).
#   • S2: NAR DIFFERENTIAL — td-builder's own NAR serializer + SHA-256
#     (`nar-hash`) must agree with the hash the DAEMON recorded in its DB
#     (query-path-info via tests/td-builder-nar.scm, printing NAR=<path> <hash>
#     pairs) for (1) a constructed fixture covering every node type and
#     framing edge (executable bit, dangling symlink, empty file/dir,
#     codepoint-order sort stress, pad-to-8 content lengths) and (2)
#     td-builder's own output. This is open question 2 settled by test: the
#     serialization the eventual builder registers outputs with is bit-for-bit
#     the daemon's. Verified-red (driven before this leg may land):
#     ordering/padding defects in nar.rs each red it — evidence in
#     plan/td-builder.md.
#   • S3: BUILD DIFFERENTIAL — td-builder parses the ATerm drv, executes its
#     builder in a fresh user namespace (uid 30001, staged store rbind, the
#     daemon's env contract — plan/td-builder.md Q4) and registers the output
#     (v1 record — Q3). Asserted against the daemon, which builds the SAME
#     deterministic drv (tests/td-builder-s3-drvs.scm): same store path,
#     NAR hash equal to the daemon's RECORDED hash, NAR size, references set
#     (an input ref + a self-ref — the scan must find both) and deriver all
#     equal; plus the rootless gate's isolation assert on a separate
#     namespace-sensitive probe drv (built td-side only — its output records
#     uid_map and can never be a differential subject).
#   • S4: SYSTEM-IMAGE DIFFERENTIAL — the §7.1 acceptance subject: td-builder
#     rebuilds the `build` gate's qcow2 image drv itself
#     (tests/td-builder-s4-drv.scm prints the oracle facts the root daemon
#     recorded when it built the SAME drv) and must register equal fields at
#     the same path — store path, NAR hash (recorded AND independently
#     re-hashed), NAR size, references set (compared even if empty) and
#     deriver. This is what forces the sandbox past S3's minimum: the image
#     builder is a real multi-process Guile build (mke2fs/genimage tree) that
#     honestly reds on any missing piece of the daemon's chroot contract.
# OFFLINE PRECONDITION (DESIGN §5): the pinned Rust closure must be warm in the
# host store — the loop fetches nothing. Two-step lower-then-realise (repl ->
# guix build) for an honest exit status, as in the other gates.
HEAVY_GATES += td-builder
td-builder:
	@echo ">> td-builder: reproducible offline build (S1) + NAR differential (S2) + build differential (S3) + system-image differential (S4)"
	@set -euo pipefail; \
	drv=`$(GUIX) repl $(LOAD) tests/td-builder-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the td-builder derivation" >&2; exit 1; }; \
	echo ">> td-builder derivation: $$drv"; \
	start=`date +%s`; \
	out=`$(GUIX) build "$$drv"`; \
	elapsed=$$(( `date +%s` - start )); \
	test -n "$$out" || { echo "ERROR: the td-builder build produced no output path" >&2; exit 1; }; \
	echo ">> check: reproducibility of the td-builder binary (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$drv"; \
	echo ">> run: the compiled binary must print its sentinel"; \
	"$$out/bin/td-builder" | grep -Eq '^td-builder [0-9.]+ ok$$' \
	  || { echo "FAIL: the compiled td-builder did not print its sentinel (or exited nonzero) — the toolchain did not produce a working binary." >&2; exit 1; }; \
	echo ">> S2: NAR differential — td-builder nar-hash vs the daemon's recorded hash"; \
	pairs=`$(GUIX) repl $(LOAD) tests/td-builder-nar.scm 2>/dev/null | sed -n 's/^NAR=//p'`; \
	test -n "$$pairs" || { echo "ERROR: could not compute the oracle NAR pairs (tests/td-builder-nar.scm)" >&2; exit 1; }; \
	n=0; \
	while read -r p expect; do \
	  test -n "$$p" -a -n "$$expect" || { echo "ERROR: malformed oracle pair: '$$p $$expect'" >&2; exit 1; }; \
	  have=`"$$out/bin/td-builder" nar-hash "$$p"` \
	    || { echo "FAIL: td-builder nar-hash failed on $$p" >&2; exit 1; }; \
	  test "$$have" = "sha256:$$expect" \
	    || { echo "FAIL: NAR hash mismatch for $$p" >&2; \
	         echo "      td-builder: $$have" >&2; \
	         echo "      daemon    : sha256:$$expect" >&2; exit 1; }; \
	  echo "   nar ok ($$have): $$p"; \
	  n=$$((n + 1)); \
	done <<< "$$pairs"; \
	test "$$n" -ge 2 || { echo "FAIL: expected at least 2 oracle NAR pairs (fixture + td-builder output), got $$n" >&2; exit 1; }; \
	echo ">> S3: drv parse + sandboxed userns build differential vs the daemon"; \
	scratch="$(CURDIR)/.td-builder-scratch"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	$(GUIX) repl $(LOAD) tests/td-builder-s3-drvs.scm 2>/dev/null > "$$scratch/s3.txt"; \
	diff_drv=`sed -n 's/^DIFF_DRV=//p' "$$scratch/s3.txt"`; \
	diff_out=`sed -n 's/^DIFF_OUT=//p' "$$scratch/s3.txt"`; \
	diff_hash=`sed -n 's/^DIFF_HASH=//p' "$$scratch/s3.txt"`; \
	diff_narsize=`sed -n 's/^DIFF_NARSIZE=//p' "$$scratch/s3.txt"`; \
	diff_deriver=`sed -n 's/^DIFF_DERIVER=//p' "$$scratch/s3.txt"`; \
	probe_drv=`sed -n 's/^PROBE_DRV=//p' "$$scratch/s3.txt"`; \
	probe_out=`sed -n 's/^PROBE_OUT=//p' "$$scratch/s3.txt"`; \
	test -n "$$diff_drv" -a -n "$$diff_out" -a -n "$$diff_hash" -a -n "$$diff_narsize" \
	     -a -n "$$diff_deriver" -a -n "$$probe_drv" -a -n "$$probe_out" \
	  || { echo "ERROR: could not lower the S3 drvs (tests/td-builder-s3-drvs.scm)" >&2; exit 1; }; \
	{ sed -n 's/^DIFF_INPUT=//p;s/^PROBE_INPUT=//p' "$$scratch/s3.txt"; \
	  printf '%s\n' "$$diff_drv" "$$probe_drv"; } \
	  | xargs $(GUIX) gc -R | sort -u > "$$scratch/paths.txt"; \
	echo "   staged closure: $$(wc -l < "$$scratch/paths.txt") store items"; \
	"$$out/bin/td-builder" drv-parse "$$diff_drv" > /dev/null \
	  || { echo "FAIL: td-builder drv-parse rejected the diff drv $$diff_drv" >&2; exit 1; }; \
	echo "   isolation probe: the build must run in a fresh user namespace"; \
	"$$out/bin/td-builder" build "$$probe_drv" "$$scratch/paths.txt" "$$scratch/probe" > /dev/null \
	  || { echo "FAIL: td-builder could not build the isolation probe drv" >&2; exit 1; }; \
	map="$$scratch/probe/newstore/$${probe_out#/gnu/store/}/uid_map"; \
	test -s "$$map" || { echo "FAIL: the isolation probe recorded an empty uid_map" >&2; exit 1; }; \
	echo "   uid_map seen by the td-builder sandbox:"; sed 's/^/     /' "$$map"; \
	map_lines=`wc -l < "$$map"`; read -r map_first map_rest < "$$map"; \
	if [ "$$map_lines" -ne 1 ] || [ "$$map_first" != "30001" ]; then \
	  echo "FAIL: the td-builder build's uid_map is not a fresh per-build user" >&2; \
	  echo "      namespace mapping with the daemon's guest uid (expected the" >&2; \
	  echo "      single entry '30001 <host> 1' — build.cc defaultGuestUID; a" >&2; \
	  echo "      leading 0 means no/inherited namespace, any other uid breaks" >&2; \
	  echo "      the Q4 contract)." >&2; exit 1; \
	fi; \
	echo "   differential: td-builder rebuild vs the daemon's recorded facts"; \
	"$$out/bin/td-builder" build "$$diff_drv" "$$scratch/paths.txt" "$$scratch/diff" > "$$scratch/diff-build.txt" \
	  || { echo "FAIL: td-builder could not build the diff drv $$diff_drv" >&2; exit 1; }; \
	grep -qx "OUT=out $$diff_out" "$$scratch/diff-build.txt" \
	  || { echo "FAIL: store-path mismatch: td-builder reported '$$(cat "$$scratch/diff-build.txt")', the daemon built $$diff_out" >&2; exit 1; }; \
	reg="$$scratch/diff/registration"; \
	test -s "$$reg" || { echo "FAIL: td-builder wrote no registration record" >&2; exit 1; }; \
	grep -qx "path $$diff_out" "$$reg" \
	  || { echo "FAIL: registration path mismatch (see record below) vs $$diff_out" >&2; cat "$$reg" >&2; exit 1; }; \
	grep -qx "nar-hash sha256:$$diff_hash" "$$reg" \
	  || { echo "FAIL: NAR hash mismatch — registration '$$(sed -n 's/^nar-hash //p' "$$reg")' vs daemon 'sha256:$$diff_hash'" >&2; exit 1; }; \
	grep -qx "nar-size $$diff_narsize" "$$reg" \
	  || { echo "FAIL: NAR size mismatch — registration '$$(sed -n 's/^nar-size //p' "$$reg")' vs daemon '$$diff_narsize'" >&2; exit 1; }; \
	grep -qx "deriver $$diff_deriver" "$$reg" \
	  || { echo "FAIL: deriver mismatch — registration '$$(sed -n 's/^deriver //p' "$$reg")' vs daemon '$$diff_deriver'" >&2; exit 1; }; \
	sed -n 's/^DIFF_REF=//p' "$$scratch/s3.txt" > "$$scratch/refs.oracle"; \
	sed -n 's/^reference //p' "$$reg" > "$$scratch/refs.td"; \
	test -s "$$scratch/refs.oracle" \
	  || { echo "ERROR: the oracle recorded NO references for the diff drv — the fixture lost its discriminating refs" >&2; exit 1; }; \
	test "$$(cat "$$scratch/refs.oracle")" = "$$(cat "$$scratch/refs.td")" \
	  || { echo "FAIL: references set mismatch:" >&2; \
	       echo "      daemon recorded:" >&2; sed 's/^/        /' "$$scratch/refs.oracle" >&2; \
	       echo "      td-builder registered:" >&2; sed 's/^/        /' "$$scratch/refs.td" >&2; exit 1; }; \
	rehash=`"$$out/bin/td-builder" nar-hash "$$scratch/diff/newstore/$${diff_out#/gnu/store/}"`; \
	test "$$rehash" = "sha256:$$diff_hash" \
	  || { echo "FAIL: independent re-hash of the on-disk rebuild gives $$rehash, the daemon recorded sha256:$$diff_hash" >&2; exit 1; }; \
	echo "   rebuild equal: store path, NAR hash (registered + re-hashed), size, references (input + self), deriver"; \
	echo ">> S4: system-image differential — td-builder rebuilds the build rung's qcow2 drv"; \
	img_drv=`$(GUIX) system image $(LOAD) -t $(IMGTYPE) -d $(SYSTEM)`; \
	test -n "$$img_drv" || { echo "ERROR: could not lower the image derivation" >&2; exit 1; }; \
	echo "   target image drv: $$img_drv"; \
	img_oracle=`$(GUIX) build "$$img_drv"`; \
	test -n "$$img_oracle" || { echo "ERROR: the oracle image build produced no output path" >&2; exit 1; }; \
	TD_IMAGE_DRV="$$img_drv" $(GUIX) repl $(LOAD) tests/td-builder-s4-drv.scm 2>/dev/null > "$$scratch/s4.txt"; \
	img_out=`sed -n 's/^IMG_OUT=//p' "$$scratch/s4.txt"`; \
	img_hash=`sed -n 's/^IMG_HASH=//p' "$$scratch/s4.txt"`; \
	img_narsize=`sed -n 's/^IMG_NARSIZE=//p' "$$scratch/s4.txt"`; \
	img_deriver=`sed -n 's/^IMG_DERIVER=//p' "$$scratch/s4.txt"`; \
	test -n "$$img_out" -a -n "$$img_hash" -a -n "$$img_narsize" -a -n "$$img_deriver" \
	  || { echo "ERROR: could not read the S4 oracle facts (tests/td-builder-s4-drv.scm)" >&2; exit 1; }; \
	test "$$img_out" = "$$img_oracle" \
	  || { echo "ERROR: lowered image output ($$img_out) != realized oracle output ($$img_oracle)" >&2; exit 1; }; \
	{ sed -n 's/^IMG_INPUT=//p' "$$scratch/s4.txt"; printf '%s\n' "$$img_drv"; } \
	  | xargs $(GUIX) gc -R | sort -u > "$$scratch/s4-paths.txt"; \
	echo "   staged closure: $$(wc -l < "$$scratch/s4-paths.txt") store items"; \
	"$$out/bin/td-builder" build "$$img_drv" "$$scratch/s4-paths.txt" "$$scratch/s4" > "$$scratch/s4-build.txt" \
	  || { echo "FAIL: td-builder could not build the image drv $$img_drv" >&2; exit 1; }; \
	grep -qx "OUT=out $$img_out" "$$scratch/s4-build.txt" \
	  || { echo "FAIL: store-path mismatch: td-builder reported '$$(cat "$$scratch/s4-build.txt")', the daemon built $$img_out" >&2; exit 1; }; \
	s4reg="$$scratch/s4/registration"; \
	test -s "$$s4reg" || { echo "FAIL: td-builder wrote no registration record for the image" >&2; exit 1; }; \
	grep -qx "path $$img_out" "$$s4reg" \
	  || { echo "FAIL: image registration path mismatch (see record below) vs $$img_out" >&2; cat "$$s4reg" >&2; exit 1; }; \
	grep -qx "nar-hash sha256:$$img_hash" "$$s4reg" \
	  || { echo "FAIL: image NAR hash mismatch — registration '$$(sed -n 's/^nar-hash //p' "$$s4reg")' vs daemon 'sha256:$$img_hash'" >&2; exit 1; }; \
	grep -qx "nar-size $$img_narsize" "$$s4reg" \
	  || { echo "FAIL: image NAR size mismatch — registration '$$(sed -n 's/^nar-size //p' "$$s4reg")' vs daemon '$$img_narsize'" >&2; exit 1; }; \
	grep -qx "deriver $$img_deriver" "$$s4reg" \
	  || { echo "FAIL: image deriver mismatch — registration '$$(sed -n 's/^deriver //p' "$$s4reg")' vs daemon '$$img_deriver'" >&2; exit 1; }; \
	sed -n 's/^IMG_REF=//p' "$$scratch/s4.txt" > "$$scratch/s4-refs.oracle"; \
	sed -n 's/^reference //p' "$$s4reg" > "$$scratch/s4-refs.td"; \
	test "$$(cat "$$scratch/s4-refs.oracle")" = "$$(cat "$$scratch/s4-refs.td")" \
	  || { echo "FAIL: image references set mismatch:" >&2; \
	       echo "      daemon recorded:" >&2; sed 's/^/        /' "$$scratch/s4-refs.oracle" >&2; \
	       echo "      td-builder registered:" >&2; sed 's/^/        /' "$$scratch/s4-refs.td" >&2; exit 1; }; \
	img_rehash=`"$$out/bin/td-builder" nar-hash "$$scratch/s4/newstore/$${img_out#/gnu/store/}"`; \
	test "$$img_rehash" = "sha256:$$img_hash" \
	  || { echo "FAIL: independent re-hash of the on-disk image rebuild gives $$img_rehash, the daemon recorded sha256:$$img_hash" >&2; exit 1; }; \
	echo "   image rebuild equal: store path, NAR hash (registered + re-hashed), size, references, deriver"; \
	chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; \
	echo ">> closure size:"; $(GUIX) size "$$out" | tail -n1; \
	echo "   compile wall-clock: $${elapsed}s (first run; warm store thereafter)"; \
	echo "PASS: reproducible offline build (S1); NAR serialization bit-for-bit equal to the daemon's recorded hashes across $$n items (S2); the userns sandbox rebuild registers the daemon's exact facts at the same store path and builds in a fresh user namespace (S3); td-builder rebuilds the SYSTEM IMAGE drv itself, daemon-equal on every recorded field (S4)."
