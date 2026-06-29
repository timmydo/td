# oci-native (system-image-native track; move-off-Guile §5 / north-star priority 3).
# The td-NATIVE replacement for `guix system image -t docker`: instead of lowering an
# OCI image through guix's `(gnu system image)` Guile, `td-builder oci-image-closure`
# computes a store path's closure (builder/src/store_db_read — NO guix process) and packs
# it into a docker-archive itself (builder/src/oci.rs — a deterministic, zero-dep ustar +
# manifest/config writer). This gate proves that constructed image is REAL and WORKS,
# entirely with DURABLE assertions (no guix byte-identity oracle — td's construction is
# proven by behavior + its own reproducibility):
#   • skopeo (a foreign OCI implementation) `copy docker-archive:` loads it and yields a
#     sha256 manifest digest — the bytes are a valid OCI image;
#   • crun RUNS it rootless with NO host store bound (the image carries its own closure)
#     and the entrypoint emits "Hello, world!" — the userspace executes;
#   • INTRINSIC reproducibility: packing the same closure twice is byte-identical (prime
#     directive 1, proven by td itself — not `guix build --check`);
#   • self-discrimination: a bogus exec in the same image fails (the green discriminates).
# Scope: the IMAGE CONSTRUCTION moves off Guile here. The package + toolchain BYTES stay
# guix (retired last — the /td/store source-bootstrap); `hello` is realized as the seed
# (the same way the corpus gates realize theirs), and its run-time closure is what td packs.
SYSTEM_GATES += oci-native
oci-native:
	@echo ">> oci-native: td-builder builds a working OCI image from a store closure (no guix system image); skopeo loads it, crun runs it, reproducible"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; tb="$$TB"; \
	case "$$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($$tb)" >&2; exit 1 ;; esac; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	echo ">> realize the seed package (hello) — the retired-last guix bytes td packs"; \
	hello=`$(GUIX) build hello`; \
	test -n "$$hello" -a -x "$$hello/bin/hello" || { echo "ERROR: could not realize hello" >&2; exit 1; }; \
	skopeo=`$(GUIX) build skopeo`/bin/skopeo; \
	crun=`$(GUIX) build crun`/bin/crun; \
	scratch="$(CURDIR)/.oci-native-scratch"; chmod -R u+w "$$scratch" 2>/dev/null || true; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	printf '{"repoTag":"td-hello:latest","env":["PATH=/bin"],"entrypoint":["%s/bin/hello"]}' "$$hello" > "$$scratch/config.json"; \
	echo ">> td-builder oci-image-closure (td reads /var/guix/db, packs hello's closure — no guix system image)"; \
	"$$tb" oci-image-closure /var/guix/db/db.sqlite /gnu/store "$$scratch/config.json" "$$scratch/img1.tar" "$$hello" \
	  || { echo "FAIL: td-builder oci-image-closure failed" >&2; exit 1; }; \
	echo ">> INTRINSIC reproducibility: pack the same closure again, assert byte-identical"; \
	"$$tb" oci-image-closure /var/guix/db/db.sqlite /gnu/store "$$scratch/config.json" "$$scratch/img2.tar" "$$hello" \
	  || { echo "FAIL: second oci-image-closure failed" >&2; exit 1; }; \
	h1=`sha256sum < "$$scratch/img1.tar" | cut -d" " -f1`; \
	h2=`sha256sum < "$$scratch/img2.tar" | cut -d" " -f1`; \
	test -n "$$h1" -a "$$h1" = "$$h2" \
	  || { echo "FAIL: the td-native image is NOT reproducible (sha256 $$h1 != $$h2)" >&2; exit 1; }; \
	echo "   ok: byte-identical across two packs — sha256 $$h1 (td's own reproducibility, no guix oracle)"; \
	echo ">> behavioral: skopeo loads it + crun runs hello"; \
	SKOPEO="$$skopeo" CRUN="$$crun" sh tests/oci-native-check.sh "$$scratch/img1.tar" "$$hello/bin/hello" "Hello, world!"; \
	rm -rf "$$scratch"
