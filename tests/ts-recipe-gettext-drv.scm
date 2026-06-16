;; tests/ts-recipe-gettext-drv.scm — lower the derivations the `corpus-gettext` gate
;; builds + --checks (DESIGN §7.1 input-recipes; reconstruct nano's direct input
;; gettext-minimal). Multi-output (out + doc), like ts-recipe-libatomic-drv.scm,
;; and emits the td-check build-closure seed (TD_IN) like ts-recipe-gzip-drv.scm.
;;
;; Emits: TD_DRV / ORACLE_DRV / TD_OUT (the "out" output) / ORACLE_OUT, and TD_IN=
;; per direct-input output path (the build-closure seed for `td-builder check`).
;; The recipe JSON arrives via TD_RECIPE_GETTEXT_JSON (tsc -> boa -> recipe()).
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (ice-9 format)
             (srfi srfi-1)
             (gnu packages)                 ;the ORACLE (specification->package gettext-minimal)
             (system td-recipe))

(define (env-json name)
  (let ((v (getenv name)))
    (when (or (not v) (string=? v ""))
      (format (current-error-port)
              "ts-recipe-gettext-drv: ~a not set (need the emitted recipe JSON)~%" name)
      (exit 2))
    v))

(define (input-output-paths drv)
  (append-map (lambda (i)
                (let ((d (derivation-input-derivation i)))
                  (map (lambda (o) (derivation->output-path d o))
                       (derivation-input-sub-derivations i))))
              (derivation-inputs drv)))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let ((td     (package-derivation store
                  (json-recipe->package (env-json "TD_RECIPE_GETTEXT_JSON")) #:graft? #f))
        (oracle (package-derivation store (specification->package "gettext-minimal") #:graft? #f)))
    (format #t "TD_DRV=~a~%" (derivation-file-name td))
    (format #t "ORACLE_DRV=~a~%" (derivation-file-name oracle))
    ;; the "out" output (gettext-minimal is multi-output: out + doc).
    (format #t "TD_OUT=~a~%" (derivation->output-path td))
    (format #t "ORACLE_OUT=~a~%" (derivation->output-path oracle))
    (for-each (lambda (p) (format #t "TD_IN=~a~%" p)) (input-output-paths td))))
