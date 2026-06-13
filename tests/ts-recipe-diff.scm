;; tests/ts-recipe-diff.scm — corpus-independence differential (DESIGN §7.1,
;; Phase 2 of the §5 move-off-Guile goal).
;;
;; The capstone of the CORPUS axis, modeled on tests/ts-diff.scm (the SURFACE
;; axis): a package recipe authored in TypeScript (tests/ts/recipe-hello.ts),
;; transpiled by tsc and evaluated by the boa evaluator — the `corpus` rung runs
;; that front-end and passes the emitted recipe JSON in via the environment —
;; lowers through the generic Guile recipe bridge (system td-recipe) to the SAME
;; derivation as the pinned Guix corpus's own `hello` (the §2.5 oracle). The
;; recipe DATA comes from the TS surface; the Guile bridge is the retire-last
;; lowering target (§5).
;;
;; SELF-DISCRIMINATING, like ts-diff:
;;   (a) CONVERGE     — the TS recipe lowers to the SAME derivation as the corpus
;;       oracle (store-path-equal ⇒ NAR-hash-equal).
;;   (b) DISCRIMINATE — a perturbed TS recipe (one wrong byte in the source hash)
;;       lowers to a DIFFERENT derivation, so the differential can never rot into a
;;       vacuous pass. It also proves convergence is LOAD-BEARING on the TS-declared
;;       upstream coordinate.
;;
;; Derivation-level and build-free (`#:graft? #f`). The matching BUILD + `--check`
;; (prime directive 1) is the rest of the `corpus` rung. Run as a repl SCRIPT so
;; the process exit status is the test result.
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (ice-9 format)
             (gnu packages base)             ;the ORACLE — the only corpus import
             (system td-recipe))

(define (env-json name)
  (let ((v (getenv name)))
    (when (or (not v) (string=? v ""))
      (format #t "FAIL: ~a not set — the corpus rung must pass the emitted recipe \
JSON (tsc -> boa -> recipe()).~%" name)
      (exit 2))
    v))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (pkg-drv pkg)
    (derivation-file-name (package-derivation store pkg #:graft? #f)))

  (let* ((oracle    (pkg-drv hello))
         (candidate (pkg-drv (json-recipe->package (env-json "TD_RECIPE_JSON"))))
         (perturbed (pkg-drv (json-recipe->package (env-json "TD_RECIPE_PERTURBED_JSON"))))
         (converge?     (string=? oracle candidate))
         (discriminate? (not (string=? oracle perturbed))))

    (format #t "~%== corpus-independence differential: TS recipe (tsc -> boa -> bridge) vs. Guix corpus ==~%")
    (format #t "  oracle    (gnu packages base hello)    : ~a~%" oracle)
    (format #t "  candidate (recipe-hello.ts)            : ~a~%" candidate)
    (format #t "  perturbed (recipe-perturbed.ts)        : ~a~%" perturbed)
    (format #t "~%  (a) converge      (candidate == oracle) : ~a~%" converge?)
    (format #t "  (b) discriminate  (perturbed != oracle) : ~a~%~%" discriminate?)

    (cond
     ((not converge?)
      (format #t "FAIL: the TS-authored recipe does NOT reproduce the corpus \
oracle's derivation — the reconstructed recipe diverges from the package it \
claims to be.~%")
      (exit 1))
     ((not discriminate?)
      (format #t "FAIL: differential is vacuous — a perturbed TS recipe (wrong \
source hash) did NOT change the derivation. The differential has lost \
discriminating power.~%")
      (exit 1))
     (else
      (format #t "PASS: the TS-authored recipe lowers store-path-identical to the \
Guix corpus oracle, and a perturbed recipe diverges.~%")
      (exit 0)))))
