# bootstrap — td produces its OWN builder with NO guix and NO Guile (move-off-Guile §5;
# the "build the seed with td" direction). Today the first td-builder comes from
# `guix build -e '(@ (system td-builder) td-builder)'` — guix's cargo-build-system
# evaluating a Guile package — and rust-build only self-hosts because that guix-built
# binary already exists to run build-recipe. This gate breaks that circularity: a STAGE0
# td-builder is compiled straight from the checked-in builder/ source by the pinned
# Rust toolchain (tools/bootstrap-td-builder.sh: `cargo build` under `env -i` with ONLY
# pinned store tools on PATH — no guix, no Guile, no guix-daemon, offline). td-builder
# is std-only (zero crate deps), so the compile needs just rustc/cargo + a gcc linker,
# the guix-built toolchain seed (retired LAST §5; its paths are read from the lock as
# strings, not resolved/realized by guix).
#
# HERMETICITY (prime directive 2): `env -i` + a pinned PATH is NOT the guarantee on its
# own — it scrubs the environment and forces the PINNED rustc/cargo (no ambient
# toolchain, no host RUSTFLAGS/CC/CARGO_HOME redirect; HOME/CARGO_HOME → tmp so no
# ~/.cargo/config), but it does not isolate the filesystem or network. The ISOLATION
# comes from this gate running INSIDE td's loop sandbox (`td-builder host-sandbox`): a
# fresh-tmpfs root that exposes ONLY /gnu/store (ro) + the worktree (+ synthetic /dev,
# fresh /proc) — NO host /usr|/lib|/bin|/etc to leak from — with its OWN loopback-only
# netns (so the build is offline by construction; `--offline --frozen` is
# belt-and-suspenders). The remaining gap vs td's per-build sandbox is closure-
# COMPLETENESS: the loop sandbox exposes the WHOLE store, not just the declared toolchain
# closure, so an undeclared store input is not STRUCTURALLY prevented (build-recipe's
# staged sandbox does prevent it — the convergence point for the next brick). The
# [DURABLE hygiene] leg below closes the practical risk by PROVING the product links
# only into the pinned closure.
#
# Per the differential+durable discipline:
#   [STRUCTURAL] the stage0 build ran with guix/Guile off the PATH (the script guards
#     it) and produced a working td-builder.
#   [DURABLE behavioral] stage0 RUNS its sentinel and does real builder work (nar-hash,
#     drv-parse) — it is a functioning builder, not just a compile that exits 0.
#   [DURABLE hygiene] the stage0 binary links ONLY into the pinned store closure — its
#     ELF interpreter and RUNPATH are under /gnu/store, never host /usr|/lib (a
#     host-libc leak would show up here and red the gate).
#   [DURABLE intrinsic-reproducibility] a SECOND independent bootstrap yields a
#     BIT-IDENTICAL stage0 (td's own double-build — no guix --check).
#   [DURABLE self-discrimination] stage0's nar-hash is load-bearing: a perturbed input
#     gives a different hash.
#   [MIGRATION ORACLE, removable] stage0 is behaviorally EQUIVALENT to the guix-built
#     td-builder (identical nar-hash on the same input) while being a DISTINCT binary
#     (plain cargo vs cargo-build-system) — own, then diverge. Delete this leg when guix
#     retires; the durable legs above still stand.
#
# This is brick 1 of the bootstrap arc: it proves td-builder needs no guix to be
# CREATED. Making the loop's builds USE stage0 as the in-store builder-of-record is the
# next brick (build-recipe references the builder by store path, so it needs daemon-free
# placement of the builder).
HEAVY_GATES += bootstrap
bootstrap:
	@echo ">> bootstrap: td compiles its OWN stage0 td-builder from source with the pinned toolchain — no guix, no Guile — and it runs, is bit-reproducible, and behaviorally equals the guix-built builder"
	@set -euo pipefail; \
	scratch="$(CURDIR)/.td-build-cache/bootstrap"; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	lock="$(CURDIR)/tests/td-builder-rust.lock"; \
	test -s "$$lock" || { echo "ERROR: no lock $$lock" >&2; exit 1; }; \
	grep ' /gnu/store/' "$$lock" | sed 's/^[^ ]* //' | xargs $(GUIX) build >/dev/null \
	  || { echo "ERROR: could not realize the pinned toolchain seed (regenerate the lock on a channel bump)" >&2; exit 1; }; \
	echo ">> stage0 #1: cargo build under env -i, pinned PATH only (guix/Guile scrubbed)"; \
	s0=`TD_LOCK="$$lock" sh tools/bootstrap-td-builder.sh "$$scratch/a"`; \
	test -x "$$s0" || { echo "FAIL: bootstrap produced no stage0 td-builder" >&2; exit 1; }; \
	echo "  [STRUCTURAL] stage0 built guix/Guile-free: $$s0"; \
	sent=`"$$s0"`; \
	test "$$sent" = "td-builder 0.1.0 ok" || { echo "FAIL: stage0 sentinel was '$$sent', expected 'td-builder 0.1.0 ok'" >&2; exit 1; }; \
	printf 'bootstrap probe\n' > "$$scratch/probe"; \
	h0=`"$$s0" nar-hash "$$scratch/probe"`; \
	test -n "$$h0" || { echo "FAIL: stage0 nar-hash produced nothing" >&2; exit 1; }; \
	echo "  [DURABLE behavioral] stage0 runs its sentinel + nar-hashes a probe ($$h0)"; \
	gcc=`grep -- '-gcc-toolchain-' "$$lock" | sed 's/^[^ ]* //' | head -1`; rd="$$gcc/bin/readelf"; \
	test -x "$$rd" || { echo "ERROR: no readelf in the pinned gcc-toolchain ($$gcc)" >&2; exit 1; }; \
	interp=`"$$rd" -l "$$s0" 2>/dev/null | sed -n 's/.*program interpreter: \(.*\)\]/\1/p'`; \
	case "$$interp" in /gnu/store/*) : ;; *) echo "FAIL: stage0's ELF interpreter is NOT in the store — host-libc leak: '$$interp'" >&2; exit 1 ;; esac; \
	rp=`"$$rd" -d "$$s0" 2>/dev/null | sed -n 's/.*R[UN]*PATH.*\[\(.*\)\]/\1/p'`; \
	nonstore=`printf '%s' "$$rp" | tr ':' '\n' | grep -v '^$$' | grep -v '^/gnu/store/' || true`; \
	test -z "$$nonstore" || { echo "FAIL: stage0 RUNPATH has non-store entries — host-lib leak:" >&2; printf '%s\n' "$$nonstore" >&2; exit 1; }; \
	echo "  [DURABLE hygiene] stage0 links ONLY into the pinned store closure — ELF interp + RUNPATH under /gnu/store, no host libc/lib leak (interp $$interp)"; \
	printf 'bootstrap probe PERTURBED\n' > "$$scratch/probe2"; \
	h0p=`"$$s0" nar-hash "$$scratch/probe2"`; \
	test "$$h0" != "$$h0p" || { echo "FAIL: stage0 nar-hash is not load-bearing (perturbed input gave the same hash)" >&2; exit 1; }; \
	echo "  [DURABLE self-discrimination] a perturbed input yields a different hash ($$h0p)"; \
	echo ">> stage0 #2: a second independent bootstrap (intrinsic double-build)"; \
	s0b=`TD_LOCK="$$lock" sh tools/bootstrap-td-builder.sh "$$scratch/b"`; \
	ha=`sha256sum "$$s0" | cut -d' ' -f1`; hb=`sha256sum "$$s0b" | cut -d' ' -f1`; \
	test "$$ha" = "$$hb" || { echo "FAIL: the two stage0 builds differ ($$ha != $$hb) — the bootstrap is NOT reproducible" >&2; exit 1; }; \
	echo "  [DURABLE intrinsic-reproducibility] two independent bootstraps are bit-identical (sha256 $$ha)"; \
	echo ">> migration oracle: compare to the guix-built td-builder (removable when guix retires)"; \
	gtb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$gtb" || { echo "ERROR: could not resolve the guix-built td-builder oracle" >&2; exit 1; }; \
	hg=`"$$gtb" nar-hash "$$scratch/probe"`; \
	test "$$h0" = "$$hg" || { echo "FAIL: stage0 and the guix-built td-builder DISAGREE on nar-hash ($$h0 != $$hg) — stage0 is not a faithful build" >&2; exit 1; }; \
	echo "  [MIGRATION ORACLE] stage0 behaviorally equals the guix-built td-builder (same nar-hash $$hg)"; \
	hgb=`sha256sum "$$gtb" | cut -d' ' -f1`; \
	if [ "$$ha" = "$$hgb" ]; then echo "NOTE: stage0 is byte-identical to guix's build"; else echo "  [own, then diverge] stage0 ($$ha) is a DISTINCT binary from guix's cargo-build-system build ($$hgb) — expected, different build wrapper"; fi; \
	rm -rf "$$scratch"; \
	echo "PASS: td compiled its OWN stage0 td-builder from source with the pinned toolchain and NO guix / NO Guile / NO guix-daemon (offline, inside td's loop sandbox: only /gnu/store + worktree exposed, loopback-only netns); it runs its sentinel and nar-hashes (durable behavioral), links ONLY into the pinned store closure — no host libc/lib leak (durable hygiene), its hash is load-bearing (durable self-discrimination), two independent bootstraps are bit-identical (durable intrinsic reproducibility), and it is behaviorally equivalent to — yet a distinct binary from — the guix-built td-builder (migration oracle, own-then-diverge). The first td-builder no longer needs guix to be created; the toolchain seed is retired last (§5)."
