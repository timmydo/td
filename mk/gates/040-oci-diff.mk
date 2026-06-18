# M5 OCI differential (DESIGN §2.4 step 5/§2.5). Same cheap, derivation-level,
# self-discriminating shape as `diff`, but the artifact is the Docker/OCI image
# derivation: prove the typed front-end drives the OCI image too, and that a
# changed config diverges. No image is built here — the bit-for-bit repro check
# is the `oci` gate below. Run as a repl SCRIPT so `(exit)` is the gate's status.
SYSTEM_GATES += oci-diff
oci-diff:
	@echo ">> oci-diff: typed front-end lowers to the same OCI image drv as the gexp"
	$(GUIX) repl $(LOAD) tests/oci-diff.scm
