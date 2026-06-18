;; tests/rust-build-drv.scm — facts for the `rust-build` gate. Lowers the
;; SELF-HOSTING rust-build derivation (td-builder built by td's OWN cargo runner,
;; system/td-build.scm `td-rust-selfhost-derivation`) and emits:
;;   DRV     the .drv (builder = the td-builder binary, arg `rust-build`)
;;   OUT     its output path (the td-built td-builder)
;;   GUIX_TB the guix cargo-build-system td-builder (the removable migration
;;           oracle — it legitimately lands at a DIFFERENT path)
;;   INPUT   each direct input's output path — the gate `guix gc -R`s these (+ the
;;           drv) to stage the closure `td-builder check` binds into its sandbox.
;; Offline like every other lowering (no substitutes / no offload).
(use-modules (guix) (guix derivations) (guix store) (guix packages)
             (srfi srfi-1)
             (system td-builder) (system td-build))

(define (input-output-paths drv)
  (append-map (lambda (input)
                (let ((idrv (derivation-input-derivation input)))
                  (map (lambda (out) (derivation->output-path idrv out))
                       (derivation-input-sub-derivations input))))
              (derivation-inputs drv)))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let* ((drv (td-rust-selfhost-derivation store))
         (out (derivation->output-path drv))
         (gtb (derivation->output-path (package-derivation store td-builder #:graft? #f))))
    (format #t "DRV=~a~%" (derivation-file-name drv))
    (format #t "OUT=~a~%" out)
    (format #t "GUIX_TB=~a~%" gtb)
    (for-each (lambda (p) (format #t "INPUT=~a~%" p)) (input-output-paths drv))))
