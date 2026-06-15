;; tests/ts-recipe-pkgconfig-drv.scm — lower the derivations the `corpus-pkgconfig`
;; gate builds + --checks (DESIGN §7.1 input-recipes: reconstruct individual INPUT
;; recipes, the move-off-Guile §5 follow-on to input-resolution). The pkg-config
;; counterpart of tests/ts-recipe-nano-drv.scm.
;;
;; Emits, for the shell gate to consume:
;;   TD_DRV     — the TS-authored recipe (recipe-pkg-config.ts, lowered through the
;;                Guile recipe bridge system td-recipe), ungrafted .drv
;;   ORACLE_DRV — the pinned corpus `pkg-config` (the §2.5 oracle), ungrafted .drv
;;   ORACLE_OUT — the corpus oracle's ungrafted output store path
;;
;; The TS recipe converges on the oracle, so TD_DRV == ORACLE_DRV and building
;; TD_DRV yields ORACLE_OUT — the gate asserts that, then proves the build
;; reproducible (`guix build --check`) and NAR-hash-equal.
;;
;; The recipe JSON arrives via TD_RECIPE_PKGCONFIG_JSON (tsc -> boa -> recipe()).
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (ice-9 format)
             (gnu packages)                 ;the ORACLE (specification->package pkg-config)
             (system td-recipe))

(define (env-json name)
  (let ((v (getenv name)))
    (when (or (not v) (string=? v ""))
      (format (current-error-port)
              "ts-recipe-pkgconfig-drv: ~a not set (need the emitted recipe JSON)~%" name)
      (exit 2))
    v))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let ((td     (package-derivation store
                  (json-recipe->package (env-json "TD_RECIPE_PKGCONFIG_JSON")) #:graft? #f))
        (oracle (package-derivation store (specification->package "pkg-config") #:graft? #f)))
    (format #t "TD_DRV=~a~%" (derivation-file-name td))
    (format #t "ORACLE_DRV=~a~%" (derivation-file-name oracle))
    (format #t "ORACLE_OUT=~a~%" (derivation->output-path oracle))))
