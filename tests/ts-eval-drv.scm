;; tests/ts-eval-drv.scm — lower the td-ts-eval package to its derivation file
;; name, for the `ts-eval` rung (DESIGN §7.1 ts-frontend, sub-task 2). Same
;; honest two-step shape as tests/td-builder-drv.scm: print the .drv so the
;; Makefile can `guix build` it (exit status honest) and `--check` it
;; (verdict-memoized). Offline contract as elsewhere (no substitutes/offload).
(use-modules (guix)
             (guix derivations)
             (guix monads)
             (system td-ts))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (format #t "DRV=~a~%"
          (derivation-file-name
           (run-with-store store (lower-object td-ts-eval)))))
