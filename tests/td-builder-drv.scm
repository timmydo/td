;; tests/td-builder-drv.scm — lower the td-builder package to a derivation and
;; print its name, so the Makefile's `td-builder` rung (S1) can realise it and
;; `guix build --check` it. Two-step lower-then-realise, like the other rungs:
;; lowering here only PRINTS the drv name; the build — and the rung's honest
;; pass/fail — happens in `guix build`, whose exit status is trustworthy (a
;; script piped into `guix repl` always exits 0 and would hide a red).
(use-modules (guix)
             (guix monads)
             (system td-builder))

(with-store store
  ;; Offline contract: forbid substitutes AND remote offloading for this store
  ;; session (`guix repl` does not read GUIX_BUILD_OPTIONS).
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let ((drv (run-with-store store (lower-object td-builder))))
    (format #t "DRV=~a~%" (derivation-file-name drv))))
