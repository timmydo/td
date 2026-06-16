;; tests/td-build-recipe-drv.scm — lower ANY reconstructed recipe through td's OWN
;; builder (system td-build), for the `td-build-corpus` gate (DESIGN §7.1
;; move-off-Guile §5; routing the corpus recipes off td-recipe.scm onto td's own
;; builder). Generic over the recipe (TD_RECIPE_JSON), unlike the gzip-specific
;; td-build-phases-drv.scm.
;;
;; The builder is the td-builder Rust binary; the recipe's configure flags + phases
;; flow to it (td-build-components sets TD_CONFIGURE_FLAGS + TD_PHASES). The output
;; is a DISTINCT store path (own builder), proven BEHAVIORALLY + structurally +
;; reproducibly, not by NAR-equality.
;;
;; Emits: TD_DRV, TD_OUT, TD_BUILDER (must be `td-builder`), TD_IN= per direct-input
;; output path (the td-check build-closure seed). Recipe JSON via TD_RECIPE_JSON.
(use-modules (guix)
             (guix derivations)
             (guix monads)
             (json)
             (srfi srfi-1)
             (ice-9 format)
             (system td-build))

(define (env-json name)
  (let ((v (getenv name)))
    (when (or (not v) (string=? v ""))
      (format (current-error-port)
              "td-build-recipe-drv: ~a not set (need the emitted recipe JSON)~%" name)
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
  (let* ((alist  (json-string->scm (env-json "TD_RECIPE_JSON")))
         (cflags (let ((v (assoc-ref alist "configureFlags")))
                   (if v (string-join (vector->list v) " ") "")))
         (drv    (td-rust-build-derivation store alist #:configure-flags cflags)))
    (format #t "TD_DRV=~a~%" (derivation-file-name drv))
    (format #t "TD_OUT=~a~%" (derivation->output-path drv))
    (format #t "TD_BUILDER=~a~%" (basename (derivation-builder drv)))
    (for-each (lambda (p) (format #t "TD_IN=~a~%" p)) (input-output-paths drv))))
