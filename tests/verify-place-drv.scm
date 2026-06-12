;; tests/verify-place-drv.scm — lower the M12 S4 verify-then-place derivations
;; for the Makefile `verify-place` rung. Prints DRV_* lines.
;;
;;   DRV_VPLACE — the target tree placed from the SIGNED registry in the
;;       placer's VERIFIED mode (--registry/--digest/--pubkey), generations 1
;;       and 2. The expected manifest digests come in via TD_DIGEST_1 /
;;       TD_DIGEST_2 (the rung obtains them from skopeo — the foreign oracle —
;;       so the placer is told what to demand, independently of the registry's
;;       own files).
;;   DRV_DIRECT — the same generations placed DIRECTLY from the docker-archive
;;       artifacts (the existing legacy path, same #:gens/#:keep as the place
;;       rung's DRV_PLACE): the oracle for the S4 differential — verified
;;       placement must yield the identical tree except the image-digest
;;       representation.
;;
;;   IMG_1 / LABEL_1 — gen-1's docker-archive artifact path and root label,
;;       for tests/verify-place-check.sh's crafted-image control (n4).
;;
;; Run as a repl SCRIPT (not piped via STDIN) — see tests/typed-diff.scm.
(use-modules (guix store)
             (guix derivations)
             (ice-9 format)
             (system td-typed)
             (system td-generation)
             (system td-place))

(define (digest-env n)
  (let ((v (getenv (string-append "TD_DIGEST_" (number->string n)))))
    (unless (and (string? v) (string-prefix? "sha256:" v))
      (format #t "ERROR: TD_DIGEST_~a is not a sha256: digest (~s)~%" n v)
      (exit 1))
    (cons n v)))

(with-store store
  ;; Offline contract (triage): no substitutes, no remote offloading (guix repl
  ;; ignores GUIX_BUILD_OPTIONS) — see tests/typed-diff.scm / check.sh.
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (drv obj) (derivation-file-name (run-with-store store obj)))

  (format #t "DRV_VPLACE=~a~%"
          (drv (td-registry-placed-tree
                #:gens '(1 2)
                #:digests (list (digest-env 1) (digest-env 2))
                #:keep 10)))
  (format #t "DRV_DIRECT=~a~%"
          (drv (td-placed-tree #:gens '(1 2) #:keep 10)))

  (format #t "IMG_1=~a~%"
          (derivation->output-path
           (run-with-store store
                           (td-generation-image (td-config #:generation 1)))))
  (format #t "LABEL_1=~a~%"
          (td-config-effective-root-label (td-config #:generation 1))))
