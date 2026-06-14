;; tests/td-drv-assemble-drv.scm — emit the build-derivation SPEC + the oracle for
;; the `td-drv-assemble` rung (DESIGN §7.1). Guile RESOLVES the inputs (toolchain +
;; source → store paths — input resolution, stays Guix's, retired last) and writes
;; the raw spec to TD_SPEC_OUT WITHOUT calling `(derivation …)`. The ORACLE is the
;; same recipe lowered through guix's `(derivation …)` — td-builder `drv-assemble`
;; must reproduce its `.drv` byte-identically from the spec alone.
(use-modules (guix store) (guix derivations) (ice-9 format) (system td-build))

(define %recipe
  '(("name" . "hello") ("version" . "2.12.2")
    ("source" . (("uri" . "mirror://gnu/hello/hello-2.12.2.tar.gz")
                 ("sha256" . "1aqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js")))
    ("buildSystem" . "gnu")))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let ((spec-out (getenv "TD_SPEC_OUT")))
    (unless (and spec-out (not (string=? spec-out "")))
      (format (current-error-port) "td-drv-assemble-drv: TD_SPEC_OUT not set~%")
      (exit 2))
    (call-with-output-file spec-out
      (lambda (port) (write-td-build-spec store %recipe #:port port)))
    (format #t "ORACLE=~a~%"
            (derivation-file-name (td-rust-build-derivation store %recipe)))))
