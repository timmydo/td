;; tests/td-drv-add-drv.scm — lower the `td-build` hello derivation for the
;; `td-drv-add` rung (DESIGN §7.1). Writing it to the store (derivation-file-name)
;; gives the rung the skeleton `.drv` to construct + register; the rung then has
;; td-builder REGISTER its own construction via the daemon and `guix build` it.
(use-modules (guix store) (guix derivations) (ice-9 format) (system td-build))

(define %recipe
  '(("name" . "hello") ("version" . "2.12.2")
    ("source" . (("uri" . "mirror://gnu/hello/hello-2.12.2.tar.gz")
                 ("sha256" . "1aqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js")))
    ("buildSystem" . "gnu")))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (format #t "DRV=~a~%" (derivation-file-name (td-rust-build-derivation store %recipe))))
