;; tests/imperative-surface.scm — M7 helper: print the derivations the Makefile's
;; `no-guix` rung builds to prove `ship-guix? #f` is a REAL, closure-level guix-free
;; guarantee. The guarantee is TWO-LAYER (see (system td-hardening), review F1):
;;
;;   1. `guix-free-marker` — embedded in `packages`, so every bare lowering builds
;;      it. A MANIFEST-LEVEL pre-filter: catches guix reaching the closure via a
;;      manifest package. It does NOT see service-injected guix.
;;   2. `guix-free-system-gate` — a separate gate derivation over the WHOLE system
;;      closure (`operating-system-derivation`). It catches guix injected by a
;;      SERVICE (e.g. guix-service-type), which sits in the system closure but never
;;      in `operating-system-packages` — the exact hole the marker leaves open.
;;
;; Five derivations, each a deliberate, self-discriminating role:
;;
;;   * HARDENED — bare docker image of `(ship-guix? #f)` over a NON-DEFAULT manifest
;;     (base + hello). Must BUILD (the embedded marker passes) and be guix-free.
;;     Using a non-default manifest exercises the manifest path under hardening.
;;
;;   * CONTROL — bare docker image of `(ship-guix? #t)`. The discriminator: it ships
;;     guix, so the Makefile's tarball grep finds the surface (≥1). If the probe
;;     stopped finding guix, this side reddens — proving the test still tells
;;     guix-ful from guix-free.
;;
;;   * ADVERSARIAL (manifest) — bare docker image of `(ship-guix? #f)` over a manifest
;;     with a package that BYPASSES the constructor's name/propagation pre-filter
;;     (its name is not "guix" and it propagates nothing) yet keeps a RUNTIME
;;     REFERENCE to guix. Its build MUST FAIL at the embedded MARKER — proving the
;;     marker is closure-level for the manifest path, not a mere name scan.
;;
;;   * SHIPPED_GATE — whole-system gate over the actual SHIPPED `td-system`. Must
;;     BUILD (the shipped system is guix-free). This is the direct enforcement the
;;     review asked for: if system/td.scm ever drops `(delete guix-service-type)`,
;;     guix re-enters its system closure and THIS reddens — the shipped artifact is
;;     gated at the closure level, not merely by differential convergence.
;;
;;   * SVCINJ_GATE — whole-system gate over a hardened system with guix-service-type
;;     RESTORED (the review's service-injection escape). guix is back in the SYSTEM
;;     closure but not in the manifest, so the marker is blind to it; the system
;;     gate MUST FAIL at its diagnostic. Verified-RED proof that the system gate
;;     closes the service-injection hole. The Makefile asserts both adversarial
;;     builds fail with the EXPECTED diagnostic, so an unrelated error cannot green.
;;
;; The image fixtures are explicit typed configs, independent of the shipped
;; `system/td.scm` target (triage F2), so promoting the shipped default to hardened
;; never reddens those; SHIPPED_GATE deliberately DOES gate the shipped target.
;;
;; Emits `DRV_HARDENED=`, `DRV_CONTROL=`, `DRV_ADVERSARIAL=`, `DRV_SHIPPED_GATE=` and
;; `DRV_SVCINJ_GATE=` lines (the Makefile greps them out), mirroring the
;; `test`/`manifest-check` two-step lower-then-realise pattern: `guix repl` reading a
;; script from STDIN swallows exit codes, so we lower to drvs here and let `guix
;; build` carry the honest exit status.
(use-modules (guix store)
             (guix derivations)
             (guix gexp)
             (guix monads)
             (guix packages)
             (guix build-system trivial)
             ((guix licenses) #:prefix license:)
             (gnu)
             (gnu services)                    ;service
             (gnu services base)               ;guix-service-type — service injection
             (gnu system image)
             (gnu packages base)               ;hello — non-default manifest probe
             (gnu packages package-management) ;guix — the surface under test
             (system td)                       ;td-system — the SHIPPED system gated
             (system td-typed)
             (system td-hardening)             ;guix-free-system-gate (whole-system)
             (ice-9 format))

;; Adversarial fixture (review escape): a package that the td-config pre-filter
;; ACCEPTS — its name is not "guix" and it propagates nothing — but that retains a
;; RUNTIME REFERENCE to guix (a symlink into guix's store output records a store-path
;; reference), so guix enters the image's closure regardless. A name- or
;; propagation-based static check cannot catch this; the embedded closure marker
;; must. It references the EXISTING guix output (no from-source rebuild), so it
;; stays warm and offline-safe.
(define guix-runtime-ref
  (package
    (name "td-guix-runtime-ref")
    (version "0")
    (source #f)
    (build-system trivial-build-system)
    (arguments
     (list #:builder
           #~(begin
               (mkdir %output)
               ;; A symlink into guix/bin records a runtime reference to guix.
               (symlink (string-append #$guix "/bin/guix")
                        (string-append %output "/guix")))))
    (synopsis "Adversarial test fixture: retains a runtime reference to guix")
    (description "Bypasses the td-config name/propagation pre-filter (its name is
not \"guix\" and it propagates nothing) while keeping a runtime reference to guix,
to prove the embedded closure-level guix-free marker catches what a static check
cannot.")
    (home-page "https://example.invalid/td-guix-runtime-ref")
    (license license:gpl3+)))

;; Service-injection adversarial fixture (review F1): take a hardened (ship-guix? #f)
;; system — which the embedded marker certifies guix-free at the MANIFEST level —
;; and RESTORE guix-service-type. This simulates a regression that drops the
;; `(delete guix-service-type)` line: guix re-enters the SYSTEM closure (via the
;; guix-daemon shepherd service + build users) WITHOUT ever touching
;; `operating-system-packages`, so the manifest-only `guix-free-marker` cannot see
;; it. Only the whole-system `guix-free-system-gate` catches it. Its gate build MUST
;; FAIL at the gate diagnostic — the verified-RED proof that the system gate closes
;; the service-injection hole the marker leaves open.
(define service-injected-os
  (let ((hardened (td-config->operating-system (td-config #:ship-guix? #f))))
    (operating-system
      (inherit hardened)
      (services (cons (service guix-service-type)
                      (operating-system-user-services hardened))))))

(with-store store
  ;; Offline contract (triage): no substitutes, no remote offloading (guix repl
  ;; ignores GUIX_BUILD_OPTIONS). The control ships the warm guix closure; the
  ;; hardened image only SHRINKS it; the adversarial ones add the tiny runtime-ref
  ;; package / restore guix-service-type over the already-warm guix closure —
  ;; nothing cold is pulled.
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (drv obj)
    (derivation-file-name (run-with-store store (lower-object obj))))

  ;; Lower a whole-system gate (over an operating-system) to its .drv path. The
  ;; gate references `operating-system-derivation`, so its build scans the entire
  ;; system closure for bin/guix (catches service-injected guix the marker misses).
  (define (system-gate-drv os)
    (derivation-file-name (run-with-store store (guix-free-system-gate os))))

  ;; The ORDINARY public lowering — no opt-in gate. Hardened configs are gated by
  ;; the marker td-config->operating-system embeds into their package set.
  (define (docker-image-of config)
    (system-image
     (image-with-os docker-image (td-config->operating-system config))))

  (let ((hardened
         (drv (docker-image-of
               (td-config #:ship-guix? #f #:manifest (cons hello %base-packages)))))
        (control
         (drv (docker-image-of (td-config #:ship-guix? #t))))
        (adversarial
         (drv (docker-image-of
               (td-config #:ship-guix? #f
                          #:manifest (cons guix-runtime-ref %base-packages)))))
        ;; Whole-system gate over the SHIPPED system: must BUILD (td-system is
        ;; guix-free), so a guix-service regression in system/td.scm reddens here.
        (shipped-gate (system-gate-drv td-system))
        ;; Whole-system gate over the service-injection fixture: must FAIL at the
        ;; gate diagnostic (guix back in the system closure via a service).
        (svcinj-gate (system-gate-drv service-injected-os)))
    (format #t "DRV_HARDENED=~a~%" hardened)
    (format #t "DRV_CONTROL=~a~%" control)
    (format #t "DRV_ADVERSARIAL=~a~%" adversarial)
    (format #t "DRV_SHIPPED_GATE=~a~%" shipped-gate)
    (format #t "DRV_SVCINJ_GATE=~a~%" svcinj-gate)))
