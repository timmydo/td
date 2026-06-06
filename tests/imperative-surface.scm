;; tests/imperative-surface.scm — M7 helper: print the OCI image derivations the
;; Makefile's `no-guix` rung builds to prove `ship-guix? #f` is a REAL, closure-
;; level, manifest-agnostic guix-free guarantee (DESIGN §6 image-swap-only), not a
;; fixture-specific or name-based approximation.
;;
;; Four derivations, each a deliberate, self-discriminating role:
;;
;;   * HARDENED_IMAGE — the bare `(ship-guix? #f …)` docker image over a NON-DEFAULT
;;     manifest (base + hello). Built so the Makefile can `--check` its
;;     reproducibility (prime directive 1) and grep its tarball (expect 0 guix
;;     binaries). The non-default manifest exercises the manifest path under
;;     hardening (not just %base-packages).
;;
;;   * HARDENED_GATE — the SAME hardened config wrapped by `guix-free-docker-image`,
;;     i.e. the SUPPORTED gated build path. It must BUILD: success means the
;;     closure gate passed (the image is guix-free). This is the guarantee itself.
;;
;;   * CONTROL — a bare `(ship-guix? #t)` docker image (NOT gated). The discriminator:
;;     it ships guix, so the Makefile's tarball grep finds the surface (≥1). If the
;;     probe stopped finding guix, this side reddens — proving the test can still
;;     tell guix-ful from guix-free.
;;
;;   * ADVERSARIAL — the hardened config wrapped by `guix-free-docker-image` over a
;;     manifest containing a package that BYPASSES the constructor's
;;     name/propagation pre-filter (its name is not "guix" and it propagates
;;     nothing) yet keeps a RUNTIME REFERENCE to guix, so guix lands in the image
;;     closure anyway — exactly the round-3 review escape. Its build MUST FAIL at
;;     the gate. This is the verified-RED half baked in: it proves the guarantee is
;;     closure-level (catches what static checks cannot), not just a name scan. The
;;     Makefile asserts `guix build` of this derivation FAILS *at the gate*.
;;
;; The CONTROL and HARDENED fixtures are explicit typed configs, independent of the
;; shipped `system/td.scm` target (triage F2), so promoting the shipped default to
;; hardened never reddens this rung.
;;
;; Emits `DRV_HARDENED_IMAGE=`, `DRV_HARDENED_GATE=`, `DRV_CONTROL=` and
;; `DRV_ADVERSARIAL=` lines (the Makefile greps them out), mirroring the
;; `test`/`manifest-check` two-step lower-then-realise pattern: `guix repl` reading
;; a script from STDIN swallows exit codes, so we lower to drvs here and let `guix
;; build` carry the honest exit status.
(use-modules (guix store)
             (guix derivations)
             (guix gexp)
             (guix monads)
             (guix packages)
             (guix build-system trivial)
             ((guix licenses) #:prefix license:)
             (gnu)
             (gnu system image)
             (gnu packages base)               ;hello — non-default manifest probe
             (gnu packages package-management) ;guix — the surface under test
             (system td-typed)
             (system td-hardening)
             (ice-9 format))

;; Adversarial fixture (round-3 review escape): a package that the td-config
;; pre-filter ACCEPTS — its name is not "guix" and it propagates nothing — but that
;; retains a RUNTIME REFERENCE to guix (a symlink into guix's store output records a
;; store-path reference), so guix enters the image's closure regardless. A name- or
;; propagation-based static check cannot catch this; the closure gate must. It
;; references the EXISTING guix output (no from-source rebuild), so it stays warm
;; and offline-safe.
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
to prove the closure-level guix-free gate catches what a static check cannot.")
    (home-page "https://example.invalid/td-guix-runtime-ref")
    (license license:gpl3+)))

(with-store store
  ;; Offline contract (triage): no substitutes, no remote offloading (guix repl
  ;; ignores GUIX_BUILD_OPTIONS). The control is the warm default closure; the
  ;; hardened image only SHRINKS it; the adversarial one adds the tiny runtime-ref
  ;; package over the already-warm guix closure — nothing cold is pulled.
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (drv obj)
    (derivation-file-name (run-with-store store (lower-object obj))))

  (define (plain-docker-image os)
    (system-image (image-with-os docker-image os)))

  (define hardened-config
    (td-config #:ship-guix? #f #:manifest (cons hello %base-packages)))

  (let ((hardened-image
         (drv (plain-docker-image (td-config->operating-system hardened-config))))
        (hardened-gate
         (drv (td-config->guix-free-docker-image
               hardened-config #:name "td-hardened-docker-image")))
        (control
         (drv (plain-docker-image
               (td-config->operating-system (td-config #:ship-guix? #t)))))
        (adversarial
         (drv (td-config->guix-free-docker-image
               (td-config #:ship-guix? #f
                          #:manifest (cons guix-runtime-ref %base-packages))
               #:name "td-adversarial-docker-image"))))
    (format #t "DRV_HARDENED_IMAGE=~a~%" hardened-image)
    (format #t "DRV_HARDENED_GATE=~a~%" hardened-gate)
    (format #t "DRV_CONTROL=~a~%" control)
    (format #t "DRV_ADVERSARIAL=~a~%" adversarial)))
