;; tests/place-drv.scm — lower the M10.2 placed-tree derivations so the Makefile
;; `place` rung can build them, `guix build --check` them, and crack the result.
;; Prints DRV_* lines for the rung to consume.
;;
;;   DRV_PLACE  — place generations 1 and 2 with a high keep (no prune): the basic
;;       "extract /boot + per-generation menu" scenario (M10.2.1).
;;   DRV_PRUNE  — place generations 1, 2, 3 with keep=2: the prune scenario
;;       (M10.2.2). Generation 1 (oldest) must be dropped; 2 and 3 remain.
;;   IMG_<n>    — generation n's bootc image ARTIFACT path (an input of both
;;       trees, so realised by the time the rung validates them): what the
;;       place-check image-digest equality assert (M12, §2.7) hashes.
;;
;; Run as a repl SCRIPT (not piped via STDIN) — see tests/typed-diff.scm.
(use-modules (guix store)
             (guix derivations)
             (guix monads)
             (ice-9 format)
             (system td-typed)
             (system td-generation)
             (system td-place))

(with-store store
  ;; Offline contract (triage): no substitutes, no remote offloading (guix repl
  ;; ignores GUIX_BUILD_OPTIONS) — see tests/typed-diff.scm / check.sh.
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (drv obj) (derivation-file-name (run-with-store store obj)))

  (format #t "DRV_PLACE=~a~%" (drv (td-placed-tree #:gens '(1 2) #:keep 10)))
  (format #t "DRV_PRUNE=~a~%" (drv (td-placed-tree #:gens '(1 2 3) #:keep 2)))

  ;; The artifacts the trees were placed from — same td-config, so identical to
  ;; the images td-placed-tree lowered as inputs.
  (for-each (lambda (n)
              (format #t "IMG_~a=~a~%"
                      n
                      (derivation->output-path
                       (run-with-store store
                                       (td-generation-image
                                        (td-config #:generation n))))))
            '(1 2 3)))
