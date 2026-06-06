;; tests/imperative-surface.scm — M7 helper: print the OCI image derivations of
;; two typed-config FIXTURES so the Makefile's `no-guix` rung can build both,
;; `guix build --check` the hardened one, and crack their tarballs open to prove
;; the imperative `guix install` surface is removed BY CONSTRUCTION (DESIGN §6
;; parking-lot: image-swap-only).
;;
;;   * HARDENED = (td-config #:ship-guix? #f) — the surface should be ABSENT.
;;   * CONTROL  = (td-config #:ship-guix? #t) — the surface should be PRESENT.
;;
;; Both are explicit fixtures built from the typed front-end, deliberately
;; INDEPENDENT of the shipped `system/td.scm` target (triage F2): the control is
;; what gives the artifact probe its discriminating power (a guix-FUL image to
;; contrast against), and tying that to the shipped image would make this rung go
;; red the moment the shipped default is promoted to hardened. With explicit
;; fixtures the rung proves the CONSTRUCTION (ship-guix? toggles the surface)
;; regardless of what td ships, so it never blocks that promotion.
;;
;; This is the realise-and-check counterpart to the derivation-level coverage of
;; `ship-guix?` in tests/typed-coverage.scm (which proves the field diverges the
;; system drv): that rung proves the knob is WIRED; this one proves the knob does
;; the REAL WORK — the built hardened image carries no `guix`/`guix-daemon` binary.
;;
;; Emits `DRV_HARDENED=<path>` and `DRV_CONTROL=<path>` lines (the Makefile greps
;; them out), mirroring the `test`/`manifest-check` two-step lower-then-realise
;; pattern: `guix repl` reading a script from STDIN swallows exit codes, so we
;; lower to drvs here and let `guix build` carry the honest exit status.
(use-modules (guix store)
             (guix derivations)
             (guix gexp)
             (guix monads)
             (gnu)
             (gnu system image)
             (gnu packages base)        ;hello — non-default manifest probe
             (system td-typed)
             (ice-9 format))

(with-store store
  ;; Offline contract (triage): forbid substitutes AND remote offloading for this
  ;; store session (guix repl ignores GUIX_BUILD_OPTIONS). This rung BUILDS the
  ;; images, so #:offload? #f keeps the build local; #:use-substitutes? #f bars
  ;; binary substitutes. A cold fixed-output SOURCE fetch by the shared daemon is
  ;; still possible (the narrowed contract — see check.sh / DESIGN §5). The control
  ;; image is the warm default closure; the hardened one only SHRINKS it (a
  ;; subset), so nothing cold is pulled in practice.
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (oci-drv os)
    (derivation-file-name
     (run-with-store store
       (lower-object
        (system-image (image-with-os docker-image os))))))

  ;; The hardened fixture uses a NON-DEFAULT manifest (base packages + hello), not
  ;; just the default. The earlier rung only built the default-manifest hardened
  ;; image, so the artifact backstop never actually exercised the manifest path
  ;; under ship-guix? #f — the exact gap a manifest-driven exploit slipped through.
  ;; With a custom manifest here, the tarball grep proves a manifest-driven
  ;; hardened image is genuinely guix-free at the artifact level, complementing the
  ;; construction-time rejection of guix-propagating manifests in td-typed.scm.
  ;; hello's closure is tiny and warm (shared with manifest-check), so this stays
  ;; in budget. The control stays default #t to keep its guix-FUL discriminating
  ;; power and warm closure.
  (let* ((hardened-manifest (cons hello %base-packages))
         (hardened (oci-drv (td-config->operating-system
                             (td-config #:ship-guix? #f
                                        #:manifest hardened-manifest))))
         (control  (oci-drv (td-config->operating-system (td-config #:ship-guix? #t)))))
    (format #t "DRV_HARDENED=~a~%" hardened)
    (format #t "DRV_CONTROL=~a~%" control)))
