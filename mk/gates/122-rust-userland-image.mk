# rust-userland-image (system-image-native + rust-userland). SHIP a td-BUILT Rust
# userland tool through td's OWN image builder. Today system/td.scm carries the guix
# `(gnu packages rust-apps)` objects (procs/fd/ripgrep/sd/eza/bat) — guix-built bytes;
# this gate proves td can instead ship its OWN build of those tools in a td-native OCI
# image (`td-builder oci-image`, NO `guix system image`). td builds `fd` guix-free
# (tests/crate-free-build.sh — the same path the rust-fd gate asserts), computes its
# runtime closure td-natively (td-builder elf-interp/elf-rpath + store-closure over
# /var/guix/db — no guix process), lays the td binary + its guix toolchain closure into
# a rootfs, and packs it. crun then runs the td-built fd FROM the image (no host store
# bound) — its OWN bytes execute. The toolchain LIBS stay guix (retired last by the
# /td/store source-bootstrap); only the userland tool is td's bytes here.
# All-durable (behavioral + intrinsic-repro + structural + self-discrimination); NO guix
# byte-identity oracle. crun/skopeo image gate → SYSTEM tier (like oci-native). It builds
# fd itself via crate-free-build (the warm crate vendor set is host-PREP, like rust-fd).
SYSTEM_GATES += rust-userland-image
rust-userland-image:
	@echo ">> rust-userland-image: ship a td-BUILT rust tool (fd) in a td-NATIVE OCI image (td-builder oci-image, no guix system image); crun runs td's OWN fd from the image; reproducible"
	@set -euo pipefail; \
	. tests/cache-lib.sh; export TD_STAGE0_BASE="$(CURDIR)/.td-build-cache/stage0"; load_stage0; load_recipe_eval; tb="$$TB"; \
	case "$$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($$tb)" >&2; exit 1 ;; esac; \
	skopeo=`$(GUIX) build skopeo`/bin/skopeo; \
	crun=`$(GUIX) build crun`/bin/crun; \
	test -x "$$skopeo" -a -x "$$crun" || { echo "ERROR: could not resolve skopeo/crun" >&2; exit 1; }; \
	echo ">> build fd guix-free (crate-free-build — td's own cargo path, no guix build)"; \
	export GUIX="$(GUIX)" ROOT="$(CURDIR)"; \
	nsout=`sh tests/crate-free-build.sh fd fd-find-10.2.0 tests/fd.lock fd-source fd` || exit 1; \
	eval "$$nsout"; ns="$$NS"; out="$$OUT"; \
	test -x "$$ns/bin/fd" || { echo "FAIL: no td-built fd at $$ns/bin/fd" >&2; exit 1; }; \
	TB="$$tb" GUIX="$(GUIX)" SKOPEO="$$skopeo" CRUN="$$crun" ROOT="$(CURDIR)" \
	  sh tests/rust-userland-image.sh "$$ns" "$$out" fd ld-linux -- --no-ignore ld-linux /gnu/store
