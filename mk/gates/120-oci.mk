# oci (move-off-Guile §5 / north-star priority 3 — the OCI slice of "retire the Guile
# lowering", workstream C). The SHIPPED td OCI image is now td-NATIVE: instead of
# lowering the system to a Docker image through guix's `(gnu system image)` Guile
# (`guix system image -t docker`, the M5 packing), `td-builder oci-image-paths` packs
# the td SYSTEM's runtime closure into a deterministic docker-archive itself
# (builder/src/oci.rs). The IMAGE CONSTRUCTION moves off Guile here; only INPUT
# RESOLUTION — realizing the td system + listing its closure store paths — stays Guix's
# (tests/oci-system-closure.scm, a `guix repl` that calls NO image lowering and reads NO
# guix private state; the retired-last half, §5). So the guix surface SHRINKS: this
# retires the guix-image reproducibility check AND the M5 OCI differential
# (tests/oci-diff.scm, gate oci-diff) with no new guix-db-read/gc reliance (directive 6;
# see the PR's directive-3 callout for the removed gates). Proven with DURABLE
# assertions (no guix byte-identity oracle):
#   • INTRINSIC reproducibility: packing the same closure twice is byte-identical (prime
#     directive 1, proven by td itself — not `guix build --check` on a guix drv);
#   • skopeo (a foreign OCI implementation) loads the archive + yields a sha256 manifest
#     digest — the bytes are a valid OCI image;
#   • crun RUNS the image rootless with NO host store bound (it carries its own closure)
#     and the td system's shipped container runtime (`crun --version`, the package
#     system/td.scm explicitly ships) executes — the packed system userspace runs;
#   • self-discrimination: a bogus exec in the same image fails (oci-native-check.sh).
SYSTEM_GATES += oci
oci:
	@echo ">> oci: td-builder packs the td SYSTEM closure into a working OCI image (no guix system image); reproducible, skopeo loads it, crun runs the shipped runtime"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; tb="$$TB"; \
	case "$$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($$tb)" >&2; exit 1 ;; esac; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	crun=`$(GUIX) build crun`; \
	skopeo=`$(GUIX) build skopeo`/bin/skopeo; \
	test -x "$$crun/bin/crun" -a -x "$$skopeo" || { echo "ERROR: could not resolve crun/skopeo" >&2; exit 1; }; \
	scratch="$(CURDIR)/.oci-scratch"; chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	trap 'chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"' EXIT; \
	echo ">> resolve the td system runtime closure (input resolution — guix repl, retired last)"; \
	$(GUIX) repl $(LOAD) tests/oci-system-closure.scm > "$$scratch/closure.txt" 2>/dev/null \
	  || { echo "FAIL: could not resolve the td system closure" >&2; exit 1; }; \
	test -s "$$scratch/closure.txt" || { echo "FAIL: empty td system closure" >&2; exit 1; }; \
	grep -qx "$$crun" "$$scratch/closure.txt" \
	  || { echo "FAIL: crun ($$crun) is not in the td system closure — wrong entrypoint" >&2; exit 1; }; \
	echo "   resolved `wc -l < "$$scratch/closure.txt"` store paths (the td system closure)"; \
	printf '{"repoTag":"td-system:latest","env":["PATH=/bin"],"entrypoint":["%s/bin/crun"]}' "$$crun" > "$$scratch/config.json"; \
	echo ">> td-builder oci-image-paths (td-native image construction — no guix system image, no /var/guix/db)"; \
	"$$tb" oci-image-paths "$$scratch/closure.txt" /gnu/store "$$scratch/config.json" "$$scratch/img1.tar" \
	  || { echo "FAIL: td-builder oci-image-paths failed on the td system closure" >&2; exit 1; }; \
	echo ">> INTRINSIC reproducibility: pack the same closure again, assert byte-identical"; \
	"$$tb" oci-image-paths "$$scratch/closure.txt" /gnu/store "$$scratch/config.json" "$$scratch/img2.tar" \
	  || { echo "FAIL: second oci-image-paths failed" >&2; exit 1; }; \
	h1=`sha256sum < "$$scratch/img1.tar" | cut -d" " -f1`; \
	h2=`sha256sum < "$$scratch/img2.tar" | cut -d" " -f1`; \
	test -n "$$h1" -a "$$h1" = "$$h2" \
	  || { echo "FAIL: the td-native system image is NOT reproducible (sha256 $$h1 != $$h2)" >&2; exit 1; }; \
	echo "   ok: byte-identical across two packs — sha256 $$h1 (td's own reproducibility, no guix oracle)"; \
	echo ">> behavioral: skopeo loads it + crun runs the shipped container runtime from the image"; \
	SKOPEO="$$skopeo" CRUN="$$crun/bin/crun" sh tests/oci-native-check.sh "$$scratch/img1.tar" "$$crun/bin/crun" "crun version" --version; \
	rm -rf "$$scratch"
