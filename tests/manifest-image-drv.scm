;; tests/manifest-image-drv.scm — M6 helper: print the OCI image derivation of a
;; SWAPPED-manifest generation (default manifest + GNU hello), so the Makefile's
;; `manifest-check` rung can build it and `guix build --check` it bit-for-bit.
;;
;; This is the realise-and-check counterpart to the derivation-level
;; tests/manifest-diff.scm: that rung proves a changed manifest yields a
;; DIFFERENT image; this one proves that swapped image generation is itself
;; reproducible (DESIGN §6 image-swap-only; prime directive 1). The swap package
;; matches manifest-diff.scm exactly, so the drv printed here is the same
;; zmv2j4zr…-docker-image.tar.gz.drv that rung reports.
;;
;; Emits a single `DRV=<path>` line (the Makefile greps it out), mirroring the
;; `test` rung's two-step lower-then-realise pattern: `guix repl` reading a
;; script from STDIN swallows exit codes, so we lower to a drv here and let
;; `guix build` carry the honest exit status.
(use-modules (guix store)
             (guix derivations)
             (guix gexp)
             (guix monads)
             (gnu)
             (gnu system image)
             (gnu packages base)        ;hello
             (system td-typed)
             (ice-9 format))

(with-store store
  (let* ((swapped-os (td-config->operating-system
                      (td-config #:manifest (cons hello %base-packages))))
         (drv (derivation-file-name
               (run-with-store store
                 (lower-object
                  (system-image (image-with-os docker-image swapped-os)))))))
    (format #t "DRV=~a~%" drv)))
