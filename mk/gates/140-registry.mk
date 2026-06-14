# M12 S3 signed distribution: the static registry (DESIGN §2.7). The registry
# is a derivation (system/td-registry.scm): both generation images pushed by
# skopeo into ONE canonical OCI layout (shared content-addressed blob store)
# plus, per image, a one-line manifest-digest STATEMENT and its detached
# signify (ed25519) signature by the committed TEST key (tests/keys/README) —
# sign the digest, never the install ordinal; no sigstore. The gate builds it,
# `--check`s it (deterministic skopeo conversion + RFC 8032 deterministic
# signatures), and runs tests/registry-check.sh: per generation the statement
# equals the manifest digest skopeo (the foreign implementation) re-derives,
# the signature verifies with the td test pubkey, and pull-by-digest works
# from the BYTES alone (manifest + every referenced blob re-hashes to its
# digest — content addressing IS the byte-identity between pushed and pulled);
# the whole blob store re-hashes honestly; and three negative controls run
# every loop on scratch copies: unsigned (sigs stripped), tampered (one layer
# byte flipped), forged (statement rewritten, signature kept) — each must be
# rejected for its own reason. The verifier exercised here (verify_pull) is
# the same contract the placer enforces before placing (M12 S4).
HEAVY_GATES += registry
registry:
	@echo ">> registry: signed static OCI-layout distribution — push, verify statements/signatures/pull-by-digest (M12 S3)"
	@set -euo pipefail; \
	drv=`$(GUIX) repl $(LOAD) tests/registry-drv.scm 2>/dev/null | sed -n 's/^DRV_REGISTRY=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the registry derivation" >&2; exit 1; }; \
	echo ">> registry derivation: $$drv"; \
	reg=`$(GUIX) build "$$drv"`; \
	echo ">> check: reproducibility of the registry (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$drv"; \
	skopeo=`$(GUIX) build skopeo`/bin/skopeo; \
	signify=`$(GUIX) build signify`/bin/signify; \
	TD_REGISTRY="$$reg" SKOPEO="$$skopeo" SIGNIFY="$$signify" \
	TD_PUBKEY=tests/keys/td_m12_signify.pub TD_GENS="1 2" \
	  sh tests/registry-check.sh
