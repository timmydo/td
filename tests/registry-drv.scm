;; tests/registry-drv.scm — lower the M12 S3 registry derivation so the
;; Makefile `registry` rung can build it, `guix build --check` it, and hand
;; the result to tests/registry-check.sh. Prints a DRV_* line for the rung.
;;
;;   DRV_REGISTRY — the signed static registry holding generations 1 and 2
;;       (system/td-registry.scm): one canonical OCI layout + the signify-
;;       signed manifest-digest statements.
;;
;; Run as a repl SCRIPT (not piped via STDIN) — see tests/typed-diff.scm.
(use-modules (guix store)
             (guix derivations)
             (ice-9 format)
             (system td-registry))

(with-store store
  ;; Offline contract (triage): no substitutes, no remote offloading (guix repl
  ;; ignores GUIX_BUILD_OPTIONS) — see tests/typed-diff.scm / check.sh.
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (format #t "DRV_REGISTRY=~a~%"
          (derivation-file-name
           (run-with-store store (td-registry #:gens '(1 2))))))
