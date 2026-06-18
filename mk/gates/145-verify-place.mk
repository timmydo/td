# M12 S4 verify-then-place (the §7.1 acceptance). The placer's VERIFIED input
# mode (--registry/--digest/--pubkey) enforces the §2.7 pull contract BEFORE
# any staging — signed statement for the demanded digest, signify signature,
# statement states that digest, manifest blob re-hashes to it, every
# referenced blob re-hashes — then hands the decompressed layers to the
# existing placement path unchanged; the placed image-digest= records the
# VERIFIED manifest digest (the §2.7 representation move; legacy --image
# keeps the artifact sha256). Two-phase: build the registry, obtain each
# generation's manifest digest from skopeo (the FOREIGN oracle — the placer
# is told what to demand independently of the registry's own files), then
# build + `--check` the verified placed tree and validate it with
# tests/place-check.scm using digest-form TD_IMAGES. tests/verify-place-check.sh
# adds the S4 differential (verified tree == direct-placement oracle tree
# except the image-digest representation) and four negative controls every
# loop: unsigned / forged statement / tampered blob refused by the placer
# (each for its own §2.7 reason, placing nothing), and a crafted legacy image
# whose embedded identity states its own digest rejected by the
# self-reference guard.
SYSTEM_GATES += verify-place
verify-place:
	@echo ">> verify-place: placer verifies signature+digest before placing; rejects unsigned/tampered (M12 S4)"
	@set -euo pipefail; \
	reg_drv=`$(GUIX) repl $(LOAD) tests/registry-drv.scm 2>/dev/null | sed -n 's/^DRV_REGISTRY=//p'`; \
	test -n "$$reg_drv" || { echo "ERROR: could not lower the registry derivation" >&2; exit 1; }; \
	reg=`$(GUIX) build "$$reg_drv"`; \
	skopeo=`$(GUIX) build skopeo`/bin/skopeo; \
	signify_dir=`$(GUIX) build signify`/bin; \
	scratch="$(CURDIR)/.verify-place-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	d1=`"$$skopeo" --tmpdir "$$scratch" inspect --format '{{.Digest}}' "oci:$$reg/oci:gen-1"`; \
	d2=`"$$skopeo" --tmpdir "$$scratch" inspect --format '{{.Digest}}' "oci:$$reg/oci:gen-2"`; \
	rm -rf "$$scratch"; \
	case "$$d1$$d2" in *sha256:*) : ;; *) echo "ERROR: no manifest digests from skopeo" >&2; exit 1 ;; esac; \
	drvs=`TD_DIGEST_1="$$d1" TD_DIGEST_2="$$d2" $(GUIX) repl $(LOAD) tests/verify-place-drv.scm 2>/dev/null`; \
	vplace_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_VPLACE=//p'`; \
	direct_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_DIRECT=//p'`; \
	img1=`printf '%s\n' "$$drvs" | sed -n 's/^IMG_1=//p'`; \
	label1=`printf '%s\n' "$$drvs" | sed -n 's/^LABEL_1=//p'`; \
	test -n "$$vplace_drv" -a -n "$$direct_drv" -a -n "$$img1" -a -n "$$label1" \
	  || { echo "ERROR: could not lower the verify-place derivations" >&2; exit 1; }; \
	echo ">> verified placed tree derivation: $$vplace_drv"; \
	vplace=`$(GUIX) build "$$vplace_drv"`; \
	direct=`$(GUIX) build "$$direct_drv"`; \
	echo ">> check: reproducibility of the verified placed tree (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$vplace_drv"; \
	echo ">> validate the verified tree (digest-form TD_IMAGES: placed identity == verified manifest digest)"; \
	TD_PLACED="$$vplace" TD_PRESENT="1 2" TD_ABSENT="" \
	TD_IMAGES="1=$$d1 2=$$d2" \
	  $(GUIX) repl $(LOAD) tests/place-check.scm; \
	echo ">> differential + rejection legs"; \
	TD_REGISTRY="$$reg" TD_PLACER=system/td-place.sh \
	TD_PUBKEY=tests/keys/td_m12_signify.pub SIGNIFY_BIN="$$signify_dir" \
	TD_DIGEST_1="$$d1" TD_GEN1_IMG="$$img1" TD_GEN1_LABEL="$$label1" \
	TD_VPLACE="$$vplace" TD_DIRECT="$$direct" \
	  sh tests/verify-place-check.sh
