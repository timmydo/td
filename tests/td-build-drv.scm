;; tests/td-build-drv.scm — lower the derivations the `td-build` rung needs
;; (DESIGN §7.1 corpus-independence; plan/corpus-independence.md "own Rust
;; builder").
;;
;; The recipe is the SAME TS-authored one the `corpus` rung uses (recipe-hello.ts
;; → tsc → boa → JSON, passed in via TD_RECIPE_JSON). Here it is lowered TWO ways
;; for the behavioral differential:
;;   TD_BUILD_*  — through system/td-build (td's OWN Rust builder; gnu-build-system
;;                 is NOT used). TD_BUILD_BUILDER is the derivation's builder's
;;                 basename — the proof it is the Rust binary, not `guile`.
;;   ORACLE_*    — the pinned Guix corpus `hello` (§2.5 oracle), gnu-build-system.
;;                 ORACLE_BUILDER is `guile`, the contrast.
;; Ungrafted (`#:graft? #f`) on the oracle to match what the rung builds + runs.
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (ice-9 format)
             (json)
             (gnu packages base)            ;the ORACLE — the only corpus import
             (system td-build))

(define (env-json name)
  (let ((v (getenv name)))
    (when (or (not v) (string=? v ""))
      (format (current-error-port)
              "td-build-drv: ~a not set (need the emitted recipe JSON)~%" name)
      (exit 2))
    (json-string->scm v)))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let* ((recipe     (env-json "TD_RECIPE_JSON"))
         (td-drv     (td-rust-build-derivation store recipe))
         (oracle-drv (package-derivation store hello #:graft? #f)))
    (format #t "TD_BUILD_DRV=~a~%"     (derivation-file-name td-drv))
    (format #t "TD_BUILD_BUILDER=~a~%" (basename (derivation-builder td-drv)))
    (format #t "ORACLE_DRV=~a~%"       (derivation-file-name oracle-drv))
    (format #t "ORACLE_BUILDER=~a~%"   (basename (derivation-builder oracle-drv)))
    (format #t "ORACLE_OUT=~a~%"       (derivation->output-path oracle-drv))))
