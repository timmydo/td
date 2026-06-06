;; tests/imperative-surface.scm — M7 helper: print the OCI image derivation of a
;; HARDENED, guix-free generation (the default config with `ship-guix?` #f), so
;; the Makefile's `no-guix` rung can build it, `guix build --check` it, and crack
;; the realized tarball open to prove the imperative `guix install` surface is
;; physically absent (DESIGN §6 parking-lot: image-swap-only by construction).
;;
;; This is the realise-and-check counterpart to the derivation-level coverage of
;; `ship-guix?` in tests/typed-coverage.scm (which proves the field diverges the
;; system drv): that rung proves the knob is WIRED; this one proves the knob does
;; the REAL WORK — the built image carries no `guix`/`guix-daemon` binary.
;;
;; Emits a single `DRV=<path>` line (the Makefile greps it out), mirroring the
;; `test`/`manifest-check` two-step lower-then-realise pattern: `guix repl`
;; reading a script from STDIN swallows exit codes, so we lower to a drv here and
;; let `guix build` carry the honest exit status.
(use-modules (guix store)
             (guix derivations)
             (guix gexp)
             (guix monads)
             (gnu)
             (gnu system image)
             (system td-typed)
             (ice-9 format))

(with-store store
  ;; Offline contract (triage): forbid substitutes AND remote offloading for this
  ;; store session (guix repl ignores GUIX_BUILD_OPTIONS). This rung BUILDS the
  ;; hardened image, so #:offload? #f keeps the build local; #:use-substitutes? #f
  ;; bars binary substitutes. A cold fixed-output SOURCE fetch by the shared daemon
  ;; is still possible (the narrowed contract — see check.sh / DESIGN §5). Removing
  ;; guix only SHRINKS the closure (a subset of the warm default image), so nothing
  ;; cold is pulled in practice.
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (let* ((hardened-os (td-config->operating-system (td-config #:ship-guix? #f)))
         (drv (derivation-file-name
               (run-with-store store
                 (lower-object
                  (system-image (image-with-os docker-image hardened-os)))))))
    (format #t "DRV=~a~%" drv)))
