;; tests/td-build-resolved-drv.scm — the input-resolution SWAP driver (DESIGN §7.1
;; move-off-Guile; "retire input resolution", Inc.2). The td-build nano build now
;; consumes td's OWN resolution for its deps instead of Guile's specification->package.
;;
;; The recipe's declared inputs (ncurses + gettext-minimal) are resolved by
;; `td-builder resolve` over the pinned lock (the rung passes their paths in via
;; TD_RESOLVED_DEPS) and handed to system/td-build as input-SOURCES — so NO
;; specification->package runs for the deps. The nano recipe is hardcoded here, as
;; the other td-drv-* drivers hardcode hello (the SURFACE is proven by the corpus*
;; rungs; this driver is about the resolution swap).
;;
;; Emits for the rung:
;;   TD_DRV               — the td-resolved-deps nano .drv (deps as input-sources)
;;   TD_SOURCES_MATCH     — yes iff the .drv's input-sources are EXACTLY the
;;                          td-builder-resolved dep paths (the build's deps came
;;                          from td's resolution)
;;   TD_DEPS_NOT_INPUTDRVS— yes iff ncurses/gettext are NOT input-derivations (no
;;                          Guile drv resolution for the deps — the swap happened)
;;   CORPUS_DRV / CORPUS_OUT — the pinned corpus `nano` (§2.5 oracle), for the
;;                          behavioral differential the rung runs.
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (ice-9 format)
             (srfi srfi-1)
             (srfi srfi-13)
             (gnu packages)                 ;the ORACLE (corpus nano)
             (system td-build))

(define %nano-recipe
  '(("name" . "nano")
    ("version" . "8.7.1")
    ("source" . (("uri" . "mirror://gnu/nano/nano-8.7.1.tar.xz")
                 ("sha256" . "1pyy3hnjr9g0831wcdrs18v0lh7v63yj1kaf3ljz3qpj92rdrw3n")))
    ("buildSystem" . "gnu")
    ("inputs" . #("ncurses" "gettext-minimal"))))

(define (env name)
  (let ((v (getenv name)))
    (when (or (not v) (string=? v ""))
      (format (current-error-port) "td-build-resolved-drv: ~a not set~%" name)
      (exit 2))
    v))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let* ((dep-paths (filter (lambda (s) (not (string-null? s)))
                            (string-split (env "TD_RESOLVED_DEPS") #\space)))
         (td      (td-rust-build-derivation store %nano-recipe
                    #:resolved-dep-paths dep-paths))
         (td-srcs (derivation-sources td))
         (in-names (map (lambda (di)
                          (derivation-file-name (derivation-input-derivation di)))
                        (derivation-inputs td)))
         (dep-in-drvs? (or (any (lambda (n) (string-contains n "ncurses")) in-names)
                           (any (lambda (n) (string-contains n "gettext")) in-names)))
         (corpus  (package-derivation store (specification->package "nano") #:graft? #f)))
    (format #t "TD_DRV=~a~%" (derivation-file-name td))
    (format #t "TD_SOURCES_MATCH=~a~%"
            (if (lset= string=? td-srcs dep-paths) "yes" "no"))
    (format #t "TD_DEPS_NOT_INPUTDRVS=~a~%" (if dep-in-drvs? "no" "yes"))
    (format #t "CORPUS_DRV=~a~%" (derivation-file-name corpus))
    (format #t "CORPUS_OUT=~a~%" (derivation->output-path corpus))))
