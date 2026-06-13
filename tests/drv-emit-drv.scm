;; tests/drv-emit-drv.scm — lower the derivations the `drv-emit` rung checks
;; (DESIGN §7.1 evaluator-as-library).
;;
;; The subject is the `td-build` hello derivation (the own-Rust-builder drv from the
;; corpus-independence track), constructed by guix's `derivation` — the ORACLE. The
;; rung asserts td-builder re-constructs it byte-identical. A perturbed recipe (one
;; wrong byte in the upstream source hash) is lowered too: a DIFFERENT `.drv` the
;; emitter must also match, so the differential can never go vacuous.
(use-modules (guix store)
             (guix derivations)
             (ice-9 format)
             (system td-build))

(define (recipe hash)
  `(("name" . "hello")
    ("version" . "2.12.2")
    ("source" . (("uri" . "mirror://gnu/hello/hello-2.12.2.tar.gz")
                 ("sha256" . ,hash)))
    ("buildSystem" . "gnu")))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (format #t "DRV=~a~%"
          (derivation-file-name
           (td-rust-build-derivation
            store (recipe "1aqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js"))))
  (format #t "DRV_PERT=~a~%"
          (derivation-file-name
           (td-rust-build-derivation
            store (recipe "1bqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js")))))
