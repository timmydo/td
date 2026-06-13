;; tests/ts-recipe-drv.scm — lower the derivations the `corpus` rung builds +
;; --checks (DESIGN §7.1 corpus-independence, Phase 2).
;;
;; Emits, for the shell rung to consume:
;;   TD_DRV     — the TS-authored recipe (recipe-hello.ts), lowered through the
;;                Guile recipe bridge (system td-recipe), ungrafted .drv
;;   ORACLE_DRV — the pinned corpus `hello` (the §2.5 oracle), ungrafted .drv
;;   ORACLE_OUT — the corpus oracle's ungrafted output store path
;;
;; Ungrafted (`#:graft? #f`) to match tests/ts-recipe-diff.scm and to --check
;; hello's actual COMPILE reproducibility. The TS recipe converges on the oracle,
;; so TD_DRV == ORACLE_DRV and building TD_DRV yields ORACLE_OUT — the rung asserts
;; exactly that, then proves the build reproducible and NAR-hash-equal.
;;
;; The recipe JSON arrives via TD_RECIPE_JSON (tsc -> boa -> recipe()).
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (ice-9 format)
             (gnu packages base)             ;the ORACLE — the only corpus import
             (system td-recipe))

(define (env-json name)
  (let ((v (getenv name)))
    (when (or (not v) (string=? v ""))
      (format (current-error-port)
              "ts-recipe-drv: ~a not set (need the emitted recipe JSON)~%" name)
      (exit 2))
    v))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let ((td     (package-derivation store
                  (json-recipe->package (env-json "TD_RECIPE_JSON")) #:graft? #f))
        (oracle (package-derivation store hello #:graft? #f)))
    (format #t "TD_DRV=~a~%" (derivation-file-name td))
    (format #t "ORACLE_DRV=~a~%" (derivation-file-name oracle))
    (format #t "ORACLE_OUT=~a~%" (derivation->output-path oracle))))
