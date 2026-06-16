;; tests/ts-recipe-popt-drv.scm — lower the derivations the `corpus-popt` gate
;; builds + --checks (DESIGN §7.1 input-recipes: reconstruct individual recipes,
;; the move-off-Guile §5 phase frontier). The popt counterpart of
;; tests/ts-recipe-pkgconfig-drv.scm.
;;
;; Emits, for the shell gate to consume:
;;   TD_DRV     — the TS-authored recipe (recipe-popt.ts, lowered through the Guile
;;                recipe bridge system td-recipe), ungrafted .drv
;;   ORACLE_DRV — the pinned corpus `popt` (the §2.5 oracle), ungrafted .drv
;;   ORACLE_OUT — the corpus oracle's ungrafted output store path
;;
;; The TS recipe converges on the oracle, so TD_DRV == ORACLE_DRV and building
;; TD_DRV yields ORACLE_OUT — the gate asserts that, then proves the build
;; reproducible (`guix build --check`) and NAR-hash-equal.
;;
;; The recipe JSON arrives via TD_RECIPE_POPT_JSON (tsc -> boa -> recipe()).
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (ice-9 format)
             (srfi srfi-1)
             (gnu packages)                 ;the ORACLE (specification->package popt)
             (system td-recipe))

(define (env-json name)
  (let ((v (getenv name)))
    (when (or (not v) (string=? v ""))
      (format (current-error-port)
              "ts-recipe-popt-drv: ~a not set (need the emitted recipe JSON)~%" name)
      (exit 2))
    v))

;; The realized output path of every direct input of DRV — the build-closure seed
;; the `corpus-popt` gate stages for `td-builder check` (the durable reproducibility
;; leg). Mirrors tests/td-drv-build-drv.scm's input-output-paths.
(define (input-output-paths drv)
  (append-map (lambda (i)
                (let ((d (derivation-input-derivation i)))
                  (map (lambda (o) (derivation->output-path d o))
                       (derivation-input-sub-derivations i))))
              (derivation-inputs drv)))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let ((td     (package-derivation store
                  (json-recipe->package (env-json "TD_RECIPE_POPT_JSON")) #:graft? #f))
        (oracle (package-derivation store (specification->package "popt") #:graft? #f)))
    (format #t "TD_DRV=~a~%" (derivation-file-name td))
    (format #t "ORACLE_DRV=~a~%" (derivation-file-name oracle))
    (format #t "ORACLE_OUT=~a~%" (derivation->output-path oracle))
    (for-each (lambda (p) (format #t "TD_IN=~a~%" p)) (input-output-paths td))))
