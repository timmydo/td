;; tests/td-build-deps-drv.scm — lower the derivations the `td-build-deps` rung
;; needs (DESIGN §7.1 corpus-independence; the "own Rust builder, packages WITH
;; inputs" follow-on). The nano counterpart of tests/td-build-drv.scm.
;;
;; Where `td-build` builds a LEAF recipe (hello) with td's OWN Rust builder, this
;; lowers a recipe WITH build inputs (nano: ncurses + gettext-minimal) the same
;; way — through system/td-build (builder = td-builder autotools-build, NOT
;; gnu-build-system). The recipe's declared inputs are resolved from the corpus
;; (input resolution stays Guix's, retired LAST — §5) and fed to the Rust builder
;; via TD_INPUTS, so td's builder can build a package that links real deps.
;;
;; Emits, for the rung to consume:
;;   TD_BUILD_DRV     — nano via system/td-build (own Rust builder), ungrafted .drv
;;   TD_BUILD_BUILDER — its builder's basename (proof: `td-builder`, not `guile`)
;;   TD_HAS_NCURSES   — yes/no: ncurses is a direct input of the td-build .drv
;;   TD_HAS_GETTEXT   — yes/no: gettext is a direct input of the td-build .drv
;;   ORACLE_DRV       — the pinned corpus `nano` (§2.5 oracle), gnu-build-system
;;   ORACLE_BUILDER   — its builder's basename (`guile`, the contrast)
;;   ORACLE_OUT       — the corpus oracle's ungrafted output store path
;;
;; The recipe JSON arrives via TD_RECIPE_NANO_JSON (recipe-nano.ts -> tsc -> boa).
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (ice-9 format)
             (srfi srfi-1)
             (srfi srfi-13)
             (json)
             (gnu packages)                 ;the ORACLE (specification->package nano)
             (system td-build))

(define (env-json name)
  (let ((v (getenv name)))
    (when (or (not v) (string=? v ""))
      (format (current-error-port)
              "td-build-deps-drv: ~a not set (need the emitted recipe JSON)~%" name)
      (exit 2))
    (json-string->scm v)))

;; yes/no: is a derivation whose name contains NEEDLE a DIRECT input of DRV?
(define (has-input drv needle)
  (if (any (lambda (di)
             (string-contains
              (basename (derivation-file-name (derivation-input-derivation di)))
              needle))
           (derivation-inputs drv))
      "yes" "no"))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let* ((recipe     (env-json "TD_RECIPE_NANO_JSON"))
         (td-drv     (td-rust-build-derivation store recipe))
         (oracle-drv (package-derivation store (specification->package "nano")
                                         #:graft? #f)))
    (format #t "TD_BUILD_DRV=~a~%"     (derivation-file-name td-drv))
    (format #t "TD_BUILD_BUILDER=~a~%" (basename (derivation-builder td-drv)))
    (format #t "TD_HAS_NCURSES=~a~%"   (has-input td-drv "ncurses"))
    (format #t "TD_HAS_GETTEXT=~a~%"   (has-input td-drv "gettext"))
    (format #t "ORACLE_DRV=~a~%"       (derivation-file-name oracle-drv))
    (format #t "ORACLE_BUILDER=~a~%"   (basename (derivation-builder oracle-drv)))
    (format #t "ORACLE_OUT=~a~%"       (derivation->output-path oracle-drv))))
