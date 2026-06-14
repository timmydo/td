;; tests/td-drv-build-drv.scm — oracle facts for the `td-drv-build` rung
;; (DESIGN §7.1 end-to-end td build). The subject is the `td-build` hello
;; derivation (corpus-independence's own-Rust-builder drv): td emits its `.drv`
;; (#22) and td-builder EXECUTES it in its own userns sandbox, daemon-equal.
;; The daemon builds it HERE so query-path-info returns the oracle's recorded
;; facts; mirrors tests/td-builder-s3-drvs.scm / -s4-drv.scm.
;;
;; Emits: HELLO_DRV, HELLO_OUT, HELLO_HASH (recorded NAR sha256, base16),
;; HELLO_NARSIZE, HELLO_DERIVER, HELLO_INPUT (each direct input's output path).
(use-modules (guix) (guix derivations) (guix base16)
             (srfi srfi-1)
             (system td-build))

(define (input-output-paths drv)
  (append-map (lambda (input)
                (let ((idrv (derivation-input-derivation input)))
                  (map (lambda (out)
                         (derivation->output-path idrv out))
                       (derivation-input-sub-derivations input))))
              (derivation-inputs drv)))

(define %recipe
  '(("name" . "hello") ("version" . "2.12.2")
    ("source" . (("uri" . "mirror://gnu/hello/hello-2.12.2.tar.gz")
                 ("sha256" . "1aqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js")))
    ("buildSystem" . "gnu")))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let ((drv (td-rust-build-derivation store %recipe)))
    ;; Oracle-build it so the daemon records its facts.
    (build-derivations store (list drv))
    (let* ((out (derivation->output-path drv))
           (info (query-path-info store out)))
      (format #t "HELLO_DRV=~a~%" (derivation-file-name drv))
      (format #t "HELLO_OUT=~a~%" out)
      (format #t "HELLO_HASH=~a~%" (bytevector->base16-string (path-info-hash info)))
      (format #t "HELLO_NARSIZE=~a~%" (path-info-nar-size info))
      (format #t "HELLO_DERIVER=~a~%" (path-info-deriver info))
      (for-each (lambda (p) (format #t "HELLO_INPUT=~a~%" p))
                (input-output-paths drv)))))
