;; tests/corpus-drv.scm — lower the derivations the heavy `corpus` rung builds +
;; --checks (DESIGN §7.1 corpus-independence, Phase 2).
;;
;; Emits, for the shell rung to consume:
;;   TD_DRV     — td's OWN hello recipe (system td-corpus), ungrafted .drv
;;   ORACLE_DRV — the pinned corpus `hello` (the §2.5 oracle), ungrafted .drv
;;   ORACLE_OUT — the corpus oracle's ungrafted output store path
;;
;; Ungrafted (`#:graft? #f`) to match tests/corpus-diff.scm and to --check hello's
;; actual COMPILE reproducibility (grafting only rewrites references). td-hello
;; converges on the oracle, so TD_DRV == ORACLE_DRV and building TD_DRV yields
;; ORACLE_OUT — the rung asserts exactly that, then proves the build reproducible.
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (gnu packages base)             ;the ORACLE — the only corpus import
             (system td-corpus))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let ((td     (package-derivation store td-hello #:graft? #f))
        (oracle (package-derivation store hello #:graft? #f)))
    (format #t "TD_DRV=~a~%" (derivation-file-name td))
    (format #t "ORACLE_DRV=~a~%" (derivation-file-name oracle))
    (format #t "ORACLE_OUT=~a~%" (derivation->output-path oracle))))
