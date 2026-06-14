;; tests/ts-recipe-nano-diff.scm — corpus-independence differential for a recipe
;; WITH build-time inputs (DESIGN §7.1, Phase 2 of the §5 move-off-Guile goal;
;; the "packages with inputs" follow-on named in the corpus-independence entry).
;;
;; Where tests/ts-recipe-diff.scm proves a LEAF recipe (hello) converges, this
;; proves a recipe with DEPENDENCIES converges: nano (recipe-nano.ts) declares two
;; build inputs — gettext-minimal and ncurses — by their corpus package names; the
;; Guile bridge (system td-recipe) RESOLVES each from the corpus (input resolution
;; stays Guix's, retired LAST — §5) and the result lowers to the SAME derivation as
;; the pinned Guix corpus's own `nano` (the §2.5 oracle).
;;
;; SELF-DISCRIMINATING, and discriminating on the INPUT axis specifically:
;;   (a) CONVERGE          — nano (inputs declared) lowers to the corpus oracle drv
;;       (store-path-equal ⇒ NAR-hash-equal).
;;   (b) DISCRIMINATE-src  — a perturbed recipe (one wrong byte in the source hash)
;;       lowers to a DIFFERENT drv (the differential can never rot vacuous).
;;   (c) DISCRIMINATE-deps — the SAME recipe with its inputs STRIPPED lowers to a
;;       DIFFERENT drv: the declared inputs are LOAD-BEARING, not decorative — a
;;       bridge that dropped them would diverge from the oracle.
;;   (d) INPUT-EDGE        — the declared inputs really ENTERED the build: nano's
;;       lowered derivation has ncurses and gettext-minimal among its direct
;;       derivation-inputs.
;;
;; Derivation-level and build-free (`#:graft? #f`). The matching BUILD + `--check`
;; (prime directive 1) + NAR-equality is the rest of the `corpus-deps` rung. Run
;; as a repl SCRIPT so the process exit status is the test result.
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (ice-9 format)
             (srfi srfi-1)
             (srfi srfi-13)
             (json)
             (gnu packages)                 ;the ORACLE (specification->package nano)
             (system td-recipe))

(define (env-json name)
  (let ((v (getenv name)))
    (when (or (not v) (string=? v ""))
      (format #t "FAIL: ~a not set — the corpus-deps rung must pass the emitted \
recipe JSON (tsc -> boa -> recipe()).~%" name)
      (exit 2))
    v))

;; Re-serialize the recipe JSON with its "inputs" field removed (leg (c)).
(define (strip-inputs json)
  (scm->json-string (alist-delete "inputs" (json-string->scm json))))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (pkg-drv pkg) (package-derivation store pkg #:graft? #f))
  (define (drv-name pkg) (derivation-file-name (pkg-drv pkg)))

  ;; basenames of a derivation's DIRECT input derivations (the .drv name carries
  ;; the package name) — used to assert the declared input edges are present.
  (define (input-drv-basenames drv)
    (map (lambda (di)
           (basename (derivation-file-name (derivation-input-derivation di))))
         (derivation-inputs drv)))
  (define (has-input? drv needle)
    ;; string-contains returns a match index (or #f); coerce to a clean boolean.
    (and (any (lambda (n) (string-contains n needle)) (input-drv-basenames drv))
         #t))

  (let* ((recipe-json (env-json "TD_RECIPE_NANO_JSON"))
         (oracle    (drv-name (specification->package "nano")))
         (cand-pkg  (json-recipe->package recipe-json))
         (cand-drv  (pkg-drv cand-pkg))
         (candidate (derivation-file-name cand-drv))
         (perturbed (drv-name (json-recipe->package
                               (env-json "TD_RECIPE_NANO_PERTURBED_JSON"))))
         (noinputs  (drv-name (json-recipe->package (strip-inputs recipe-json))))
         (converge?     (string=? oracle candidate))
         (disc-src?     (not (string=? oracle perturbed)))
         (disc-deps?    (not (string=? oracle noinputs)))
         (has-ncurses?  (has-input? cand-drv "ncurses"))
         (has-gettext?  (has-input? cand-drv "gettext")))

    (format #t "~%== corpus-independence differential: a recipe WITH inputs (nano) vs. the Guix corpus ==~%")
    (format #t "  oracle    (gnu corpus nano)          : ~a~%" oracle)
    (format #t "  candidate (recipe-nano.ts)           : ~a~%" candidate)
    (format #t "  perturbed (wrong source hash)        : ~a~%" perturbed)
    (format #t "  no-inputs (inputs stripped)          : ~a~%" noinputs)
    (format #t "~%  (a) converge       (candidate == oracle) : ~a~%" converge?)
    (format #t "  (b) discriminate-src (perturbed != oracle) : ~a~%" disc-src?)
    (format #t "  (c) discriminate-deps (no-inputs != oracle): ~a~%" disc-deps?)
    (format #t "  (d) input edges in nano's derivation       : ncurses=~a gettext=~a~%~%"
            has-ncurses? has-gettext?)

    (cond
     ((not converge?)
      (format #t "FAIL: the TS-authored recipe (with inputs) does NOT reproduce \
the corpus oracle's derivation — the reconstructed recipe diverges from the \
package it claims to be.~%")
      (exit 1))
     ((not disc-src?)
      (format #t "FAIL: differential is vacuous — a perturbed recipe (wrong \
source hash) did NOT change the derivation.~%")
      (exit 1))
     ((not disc-deps?)
      (format #t "FAIL: the declared inputs are NOT load-bearing — stripping them \
left the derivation unchanged, so the bridge is ignoring inputs (or the package \
has none).~%")
      (exit 1))
     ((not (and has-ncurses? has-gettext?))
      (format #t "FAIL: a declared build input is missing from nano's lowered \
derivation (ncurses=~a, gettext=~a) — the recipe's inputs did not enter the \
build.~%" has-ncurses? has-gettext?)
      (exit 1))
     (else
      (format #t "PASS: a TS-authored recipe with build inputs lowers \
store-path-identical to the Guix corpus oracle; a perturbed source diverges, \
the declared inputs are load-bearing (stripping them diverges), and ncurses + \
gettext-minimal are direct inputs of nano's derivation.~%")
      (exit 0)))))
