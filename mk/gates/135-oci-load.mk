# oci-load (side-track, deferred from M10.1; plan/oci-load.md). The shipped
# images must be consumable by an INDEPENDENT OCI implementation, not just our
# own placer (`place`) and runtime (`run`). Vehicle: skopeo, chosen by the M8
# probe discipline — 0 drvs to build on the warm store vs umoci 113 and podman
# 1238 + 290 cold fetches (rejected at M8); resolved via `$(GUIX) build` so
# check.sh's package list is untouched. For BOTH the plain td image and the
# gen-1 bootc generation image (drvs shared with `oci`/`generation-image`, so
# the marginal cost is the skopeo pass, not a rebuild):
#   • `skopeo copy docker-archive:… oci:…` — the foreign stack parses the
#     archive and verifies every blob digest while writing the CANONICAL OCI
#     LAYOUT, the §2.7 identity carrier;
#   • assert `skopeo inspect` yields a `sha256:` manifest digest from that
#     layout (the registry-addressable identity M12 signs).
# NEGATIVE CONTROL, in-gate: the gen-1 archive with ONE byte incremented inside
# the inner layer.tar must be REJECTED with a digest mismatch — proves the
# green leg is a real integrity check, not mere unpacking. The corruptor
# increments (mod 256) the byte at the midpoint, so the write can never be a
# no-op, and the midpoint of the outer tar lies inside the dominant layer.tar
# blob. `--insecure-policy` disables only signature *trust policy* (M12's
# territory, no keys exist yet); blob-digest integrity stays enforced — which
# is exactly what the control proves. Scratch lives in
# $(CURDIR)/.oci-load-scratch (disk, not the sandbox tmpfs — the rootless
# lesson: layouts + the decompressed archive are several GB); kept on red for
# triage, removed on green.
HEAVY_GATES += oci-load
oci-load:
	@echo ">> oci-load: foreign OCI implementation (skopeo) loads the shipped images"
	@set -euo pipefail; \
	skopeo=`$(GUIX) build skopeo`/bin/skopeo; \
	plain_img=`$(GUIX) system image $(LOAD) -t docker $(SYSTEM)`; \
	gen1=`$(GUIX) repl $(LOAD) tests/generation-image-drv.scm 2>/dev/null | sed -n 's/^DRV_GEN1=//p'`; \
	test -n "$$gen1" || { echo "ERROR: could not lower the gen-1 bootc image derivation" >&2; exit 1; }; \
	gen1_img=`$(GUIX) build "$$gen1"`; \
	work="$(CURDIR)/.oci-load-scratch"; rm -rf "$$work"; mkdir -p "$$work"; \
	for leg in plain:$$plain_img gen1:$$gen1_img; do \
	  name=$${leg%%:*}; img=$${leg#*:}; \
	  echo ">> skopeo copy docker-archive -> oci layout ($$name): $$img"; \
	  "$$skopeo" --tmpdir "$$work" copy --insecure-policy "docker-archive:$$img" "oci:$$work/layout-$$name:td" >/dev/null; \
	  digest=`"$$skopeo" --tmpdir "$$work" inspect --format '{{.Digest}}' "oci:$$work/layout-$$name:td"`; \
	  case "$$digest" in \
	    sha256:*) echo "   manifest digest ($$name): $$digest";; \
	    *) echo "FAIL: no manifest digest from the $$name OCI layout (got: '$$digest')" >&2; exit 1;; \
	  esac; \
	done; \
	echo ">> negative control: a corrupted layer must be REJECTED"; \
	gunzip -c "$$gen1_img" > "$$work/bad.tar"; \
	off=$$(( `stat -c %s "$$work/bad.tar"` / 2 )); \
	b=`od -An -tu1 -j $$off -N1 "$$work/bad.tar" | tr -d ' '`; \
	printf "\\$$(printf '%03o' $$(( (b + 1) % 256 )))" \
	  | dd of="$$work/bad.tar" bs=1 seek=$$off count=1 conv=notrunc status=none; \
	gzip -1 "$$work/bad.tar"; \
	if "$$skopeo" --tmpdir "$$work" copy --insecure-policy "docker-archive:$$work/bad.tar.gz" \
	     "oci:$$work/layout-bad:bad" >/dev/null 2>"$$work/bad.err"; then \
	  echo "FAIL: skopeo ACCEPTED a deliberately corrupted image — the load is not an integrity check." >&2; \
	  cat "$$work/bad.err" >&2; \
	  exit 1; \
	fi; \
	grep -qi 'digest did not match' "$$work/bad.err" \
	  || { echo "FAIL: corrupted image was rejected, but NOT with a digest mismatch:" >&2; \
	       cat "$$work/bad.err" >&2; exit 1; }; \
	rm -rf "$$work"; \
	echo "PASS: foreign load green for plain + gen-1 images; corrupted layer rejected (digest mismatch)."
