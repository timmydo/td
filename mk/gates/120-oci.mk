# 4. OCI reproducibility oracle (M5) — same shape as `build`, but for the
#    Docker/OCI image: build it, then rebuild its derivation with --check
#    (bit-for-bit identical or it is a FAILING test, prime directive 1). The
#    OS closure is shared with `build`, so --check mostly re-runs the cheap
#    docker-packing step. The matching declaration also boots as a VM (M1–M4),
#    closing the north-star "one declaration, store-based + OCI" loop (DESIGN §0).
SYSTEM_GATES += oci
oci:
	@echo ">> oci: $(SYSTEM) image (docker)"
	$(GUIX) system image $(LOAD) -t docker $(SYSTEM)
	@echo ">> check: reproducibility of the OCI image derivation (verdict-memoized — tests/check-memo.sh)"
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh \
	  $$($(GUIX) system image $(LOAD) -t docker -d $(SYSTEM))
