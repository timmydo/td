# 6. M7 imperative-surface removal — image-swap-only BY CONSTRUCTION (DESIGN §6).
#    M6 made image CONTENTS manifest-driven but left the imperative mutation
#    surface: the built image still ships `guix`/`guix-daemon`, so an in-image
#    `guix install` is physically possible. The typed `ship-guix?` field removes
#    it. Review showed (a) a NAME/PROPAGATION static check cannot guarantee a
#    guix-free image — guix can still arrive via a runtime reference or a renamed
#    inherited package — and (b) an OPT-IN gate is bypassable (the bare public
#    lowering stays ungated). So the real guarantee is now a CLOSURE-LEVEL gate
#    EMBEDDED in the hardened system's package set (system/td-hardening.scm
#    `guix-free-marker`, added by td-config->operating-system when ship-guix? is #f):
#    EVERY lowering builds the profile and therefore the marker, so a hardened image
#    is guix-free OR it does not build, for ANY manifest, with no opt-in to skip.
#    This gate proves that on the BARE public path, self-discriminating, against
#    explicit typed-config fixtures (triage F2 — NOT the shipped `$(SYSTEM)` target,
#    so promoting the shipped default to hardened never reddens this gate):
#      • HARDENED = bare docker image of (ship-guix? #f, base+hello): must BUILD
#        (the embedded marker certifies it guix-free); `--check` it reproducible
#        (prime directive 1 — this IS the gated artifact, so its --check covers the
#        gate too); crack its layer.tar — NO `bin/guix`/`bin/guix-daemon`.
#      • CONTROL = bare docker image of (ship-guix? #t): assert its tarball DOES
#        contain those binaries — the discriminator: if the probe stopped finding
#        guix, or the toggle stopped mattering, this reddens, so a green proves the
#        probe tells guix-ful from guix-free.
#      • ADVERSARIAL = bare docker image of (ship-guix? #f, manifest with a package
#        that keeps a RUNTIME REFERENCE to guix) — it BYPASSES the constructor's
#        name/propagation pre-filter, so guix enters the closure undetected by any
#        static check. Its BARE build MUST FAIL *at the embedded marker*
#        (verified-red half): this proves the guarantee is closure-level AND holds
#        on the ordinary public lowering, not via an opt-in. We assert both that the
#        build fails AND that it fails with the marker's own diagnostic (so an
#        unrelated build error cannot green it).
#    Artifact/closure-level (binary-absent) is STRONGER than the deferred docker-run
#    "guix install fails" runtime check (§2.3): a binary not in the image cannot run.
#    Heaviest gate → runs last (§1.3); closures are warm (base/hello/guix already built).
#    Two-step lower-then-realise (repl → guix build) for honest exit status.
HEAVY_GATES += no-guix
no-guix:
	@echo ">> no-guix: prove ship-guix? #f is a closure-level, build-enforced guix-free guarantee (embedded, no opt-in)"
	@set -euo pipefail; \
	drvs=`$(GUIX) repl $(LOAD) tests/imperative-surface.scm 2>/dev/null`; \
	hardened_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_HARDENED=//p'`; \
	control_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_CONTROL=//p'`; \
	adversarial_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_ADVERSARIAL=//p'`; \
	shipped_gate_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_SHIPPED_GATE=//p'`; \
	svcinj_gate_drv=`printf '%s\n' "$$drvs" | sed -n 's/^DRV_SVCINJ_GATE=//p'`; \
	test -n "$$hardened_drv" -a -n "$$control_drv" -a -n "$$adversarial_drv" \
	     -a -n "$$shipped_gate_drv" -a -n "$$svcinj_gate_drv" \
	  || { echo "ERROR: could not lower the no-guix derivations" >&2; exit 1; }; \
	echo ">> hardened (bare, embedded-gate) image derivation: $$hardened_drv"; \
	echo ">> control  image derivation: $$control_drv"; \
	echo ">> adversarial (manifest) derivation: $$adversarial_drv"; \
	echo ">> shipped whole-system gate derivation: $$shipped_gate_drv"; \
	echo ">> service-injection gate derivation: $$svcinj_gate_drv"; \
	echo ">> guarantee: the BARE hardened lowering must BUILD (the embedded marker certifies it guix-free)"; \
	hardened_img=`$(GUIX) build "$$hardened_drv"`; \
	control_img=`$(GUIX) build "$$control_drv"`; \
	echo ">> check: reproducibility of the HARDENED (gated) artifact (verdict-memoized)"; \
	TD_GUIX="$(GUIX)" sh tests/check-memo.sh "$$hardened_drv"; \
	echo ">> artifact check: the imperative guix surface is ABSENT from the hardened image and PRESENT in the control"; \
	probe() { \
	  listing=`tar xzOf "$$1" --wildcards '*/layer.tar' | tar tf -` \
	    || { echo "FAIL: could not read OCI archive $$1 (artifact missing or corrupt)" >&2; exit 1; }; \
	  printf '%s\n' "$$listing" | grep -Ec '/bin/guix(-daemon)?$$' || true; \
	}; \
	in_hardened=`probe "$$hardened_img"`; \
	in_control=`probe "$$control_img"`; \
	echo "   guix/guix-daemon executables — hardened image: $$in_hardened   control image: $$in_control"; \
	test "$$in_control" -ge 1 || { echo "FAIL: the ship-guix? #t control image has NO guix binary — the probe is broken or the toggle stopped mattering; the test cannot discriminate." >&2; exit 1; }; \
	test "$$in_hardened" -eq 0 || { echo "FAIL: the hardened (ship-guix? #f) image STILL contains a guix/guix-daemon binary — the imperative surface was not removed." >&2; exit 1; }; \
	echo ">> adversarial: the BARE hardened lowering of a manifest that smuggles guix past the pre-filter (runtime ref) must FAIL at the embedded marker"; \
	adv_log=`mktemp`; \
	if $(GUIX) build "$$adversarial_drv" >"$$adv_log" 2>&1; then \
	  echo "FAIL: the adversarial ship-guix? #f image BUILT on the bare public path — the embedded marker did NOT trip; guix entered the closure undetected by both the static pre-filter and the gate." >&2; \
	  tail -20 "$$adv_log" >&2; rm -f "$$adv_log"; exit 1; \
	fi; \
	if ! grep -q "STILL contains a guix" "$$adv_log"; then \
	  echo "FAIL: the adversarial build failed, but NOT at the guix-free marker (unexpected error) — cannot credit the gate:" >&2; \
	  tail -20 "$$adv_log" >&2; rm -f "$$adv_log"; exit 1; \
	fi; \
	rm -f "$$adv_log"; \
	echo "   ok: the adversarial hardened image was REJECTED at the embedded marker on the bare public path (guix-in-closure detected)"; \
	echo ">> whole-system gate: the SHIPPED system must pass the closure-level gate (it is guix-free)"; \
	$(GUIX) build "$$shipped_gate_drv" >/dev/null; \
	echo "   ok: the shipped td-system passes the whole-system guix-free gate (a guix-service regression in system/td.scm would redden this)"; \
	echo ">> service-injection: restoring guix-service-type to a hardened system must FAIL the whole-system gate (guix re-enters the SYSTEM closure, invisible to the manifest marker)"; \
	svc_log=`mktemp`; \
	if $(GUIX) build "$$svcinj_gate_drv" >"$$svc_log" 2>&1; then \
	  echo "FAIL: the service-injection system gate BUILT — guix-service-type re-introduced guix into the system closure but the whole-system gate did NOT trip. The gate does not actually scan the folded system closure." >&2; \
	  tail -20 "$$svc_log" >&2; rm -f "$$svc_log"; exit 1; \
	fi; \
	if ! grep -q "system closure STILL contains" "$$svc_log"; then \
	  echo "FAIL: the service-injection gate failed, but NOT at the whole-system guix-free gate (unexpected error) — cannot credit the gate:" >&2; \
	  tail -20 "$$svc_log" >&2; rm -f "$$svc_log"; exit 1; \
	fi; \
	rm -f "$$svc_log"; \
	echo "   ok: service-injected guix was REJECTED at the whole-system gate (the hole the manifest-only marker leaves open is closed)"; \
	echo "PASS: ship-guix? #f is a closure-level, build-enforced guarantee — (1) the embedded MARKER refuses any manifest-injected guix on every bare lowering; (2) the whole-system GATE certifies the shipped td-system guix-free and REJECTS service-injected guix (guix-service-type restored) that the marker cannot see; and the control ships the surface, proving the probes discriminate."
