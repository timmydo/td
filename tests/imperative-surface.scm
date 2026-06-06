;; tests/imperative-surface.scm — M7 helper: print the OCI image derivations the
;; Makefile's `no-guix` rung builds to prove `ship-guix? #f` is a REAL, closure-
;; level, manifest-agnostic guix-free guarantee that holds BY CONSTRUCTION for the
;; ordinary public lowering path — not an opt-in helper a caller can skip.
;;
;; The guarantee is now EMBEDDED: `td-config->operating-system` of a hardened (#f)
;; config prepends the `guix-free-marker` (see (system td-hardening)) to the
;; system's package set, so building the profile — which EVERY lowering does —
;; builds the marker, which FAILS if guix is anywhere in the closure. So the bare
;; `(system-image (image-with-os docker-image …))` path below is itself gated; there
;; is no separate gated artifact to opt into.
;;
;; Three derivations, each a deliberate, self-discriminating role, ALL built via the
;; ordinary bare lowering:
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
;;   * ADVERSARIAL — bare docker image of `(ship-guix? #f)` over a manifest with a
;;     package that BYPASSES the constructor's name/propagation pre-filter (its name
;;     is not "guix" and it propagates nothing) yet keeps a RUNTIME REFERENCE to
;;     guix, so guix lands in the closure anyway — exactly the review escape. Its
;;     build MUST FAIL at the embedded marker. This is the verified-RED half: it
;;     proves the guarantee is closure-level and holds on the BARE public path (not
;;     a name scan, not an opt-in). The Makefile asserts the build fails with the
;;     marker's own diagnostic, so an unrelated error cannot green it.
;;
;; The fixtures are explicit typed configs, independent of the shipped
;; `system/td.scm` target (triage F2), so promoting the shipped default to hardened
;; never reddens this rung.
;;
;; Emits `DRV_HARDENED=`, `DRV_CONTROL=` and `DRV_ADVERSARIAL=` lines (the Makefile
;; greps them out), mirroring the `test`/`manifest-check` two-step lower-then-
;; realise pattern: `guix repl` reading a script from STDIN swallows exit codes, so
;; we lower to drvs here and let `guix build` carry the honest exit status.
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

(with-store store
  ;; Offline contract (triage): no substitutes, no remote offloading (guix repl
  ;; ignores GUIX_BUILD_OPTIONS). The control is the warm default closure; the
  ;; hardened image only SHRINKS it; the adversarial one adds the tiny runtime-ref
  ;; package over the already-warm guix closure — nothing cold is pulled.
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (drv obj)
    (derivation-file-name (run-with-store store (lower-object obj))))

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
                          #:manifest (cons guix-runtime-ref %base-packages))))))
    (format #t "DRV_HARDENED=~a~%" hardened)
    (format #t "DRV_CONTROL=~a~%" control)
    (format #t "DRV_ADVERSARIAL=~a~%" adversarial)))
