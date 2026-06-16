;; tests/td-build-phases-drv.scm — lower a recipe through td's OWN builder
;; (system td-build) WITH its custom phases, for the `td-build-phases` gate
;; (DESIGN §7.1 move-off-Guile §5; td's builder runs the recipe's phases in Rust).
;;
;; Unlike the corpus-* gates (which lower through gnu-build-system / td-recipe and
;; prove byte-identity to Guix), this lowers through `td-rust-build-derivation`:
;; the BUILDER is the td-builder Rust binary, and the recipe's `phases` flow to it
;; via TD_PHASES (system/td-build.scm) — td's own phase runner applies them. The
;; output has a DISTINCT store path (own builder), so the gate proves it
;; BEHAVIORALLY + structurally + reproducibly, not by NAR-equality.
;;
;; Emits: TD_DRV, TD_OUT, TD_BUILDER (the derivation builder's basename — must be
;; `td-builder`, not `guile`), and TD_IN= per direct-input output path (the
;; td-check build-closure seed). Recipe JSON via TD_RECIPE_GZIP_JSON.
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
              "td-build-phases-drv: ~a not set (need the emitted recipe JSON)~%" name)
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
  (let* ((alist  (json-string->scm (env-json "TD_RECIPE_GZIP_JSON")))
         (cflags (let ((v (assoc-ref alist "configureFlags")))
                   (if v (string-join (vector->list v) " ") "")))
         (drv    (td-rust-build-derivation store alist #:configure-flags cflags)))
    (format #t "TD_DRV=~a~%" (derivation-file-name drv))
    (format #t "TD_OUT=~a~%" (derivation->output-path drv))
    (format #t "TD_BUILDER=~a~%" (basename (derivation-builder drv)))
    (for-each (lambda (p) (format #t "TD_IN=~a~%" p)) (input-output-paths drv))))
