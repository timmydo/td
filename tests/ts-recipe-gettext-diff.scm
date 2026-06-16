;; tests/ts-recipe-gettext-diff.scm — input-recipes differential: nano's direct
;; input gettext-minimal, reconstructed with the full phase-body vocabulary
;; (DESIGN §7.1 move-off-Guile §5).
;;
;; gettext-minimal (recipe-gettext-minimal.ts) declares a doc output, a makeFlag,
;; configure flags, and TWO custom phases — patch-fixed-paths (literal substitute*
;; over file lists) and patch-tests (match var, let-which, with-fluids, find-files,
;; cons, format). Reconstructed from upstream coordinates it lowers to the SAME
;; derivation as the pinned Guix corpus's own gettext-minimal (the §2.5 oracle).
;;
;; SELF-DISCRIMINATING:
;;   (a) CONVERGE          — gettext-minimal lowers to the corpus oracle drv
;;       (store-path-equal ⇒ NAR-hash-equal).
;;   (b) DISCRIMINATE-src  — a perturbed recipe (one wrong source-hash byte) lowers
;;       to a DIFFERENT drv (never vacuous).
;;   (c) PHASES LOAD-BEARING — the SAME recipe with `phases` STRIPPED lowers to a
;;       DIFFERENT drv: the reconstructed phases (the generated modify-phases gexp)
;;       are exactly what make gettext converge, not decorative.
;;
;; Derivation-level, build-free (`#:graft? #f`). BUILD + reproducibility + behavioral
;; + NAR-equality is the rest of the `corpus-gettext` gate. Run as a repl SCRIPT.
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (ice-9 format)
             (srfi srfi-1)
             (json)
             (gnu packages)                 ;the ORACLE (specification->package gettext-minimal)
             (system td-recipe))

(define (env-json name)
  (let ((v (getenv name)))
    (when (or (not v) (string=? v ""))
      (format #t "FAIL: ~a not set — the corpus-gettext gate must pass the emitted \
recipe JSON (tsc -> boa -> recipe()).~%" name)
      (exit 2))
    v))

(define (strip-phases json)
  (scm->json-string (alist-delete "phases" (json-string->scm json))))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (drv-name pkg) (derivation-file-name (package-derivation store pkg #:graft? #f)))

  (let* ((recipe-json (env-json "TD_RECIPE_GETTEXT_JSON"))
         (oracle    (drv-name (specification->package "gettext-minimal")))
         (candidate (drv-name (json-recipe->package recipe-json)))
         (perturbed (drv-name (json-recipe->package
                               (env-json "TD_RECIPE_GETTEXT_PERTURBED_JSON"))))
         (nophases  (drv-name (json-recipe->package (strip-phases recipe-json))))
         (converge?     (string=? oracle candidate))
         (disc-src?     (not (string=? oracle perturbed)))
         (disc-phases?  (not (string=? oracle nophases))))

    (format #t "~%== input-recipes differential: nano's input gettext-minimal (full phase vocabulary) vs. the Guix corpus ==~%")
    (format #t "  oracle    (corpus gettext-minimal)   : ~a~%" oracle)
    (format #t "  candidate (recipe-gettext-minimal.ts): ~a~%" candidate)
    (format #t "  perturbed (wrong source hash)        : ~a~%" perturbed)
    (format #t "  no-phases (phases stripped)          : ~a~%" nophases)
    (format #t "~%  (a) converge        (candidate == oracle) : ~a~%" converge?)
    (format #t "  (b) discriminate-src (perturbed != oracle) : ~a~%" disc-src?)
    (format #t "  (c) phases load-bearing (no-phases != oracle): ~a~%~%" disc-phases?)

    (cond
     ((not converge?)
      (format #t "FAIL: the TS-authored gettext-minimal recipe does NOT reproduce \
the corpus oracle's derivation — the generated phase-body gexp (match vars / \
let-which / with-fluids / find-files / cons / format) is not byte-identical to the \
corpus phases.~%")
      (exit 1))
     ((not disc-src?)
      (format #t "FAIL: differential is vacuous — a perturbed recipe (wrong source \
hash) did NOT change the derivation.~%")
      (exit 1))
     ((not disc-phases?)
      (format #t "FAIL: the declared phases are NOT load-bearing — stripping \
`phases` left the derivation unchanged.~%")
      (exit 1))
     (else
      (format #t "PASS: a TS-authored recipe for nano's input gettext-minimal — with \
a doc output, makeFlags, configure flags, and two custom phases (patch-fixed-paths \
+ patch-tests, the full phase-body vocabulary) — lowers store-path-identical to the \
Guix corpus oracle; a perturbed source diverges, and the phases are load-bearing.~%")
      (exit 0)))))
