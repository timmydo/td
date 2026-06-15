;; tests/ts-recipe-popt-diff.scm — input-recipes differential: the PHASES recipe
;; rung (DESIGN §7.1 move-off-Guile §5; the phase frontier for nano's own inputs).
;;
;; Where the earlier rungs prove configure-flags / multi-URI / multi-output, this
;; proves the recipe-DSL `phases` field: popt (recipe-popt.ts) adds a custom
;; `patch-test` phase (two `substitute*` source patches) before `configure`,
;; reconstructed from upstream coordinates lowers to the SAME derivation as the
;; pinned Guix corpus's own `popt` (the §2.5 oracle). popt is the cleanest phase
;; demonstrator: its ONLY non-default argument is the one phase. The bridge lowers
;; the phase DATA to the byte-identical `(modify-phases …)` gexp the corpus writes
;; by hand. This is the prerequisite for nano's DIRECT inputs ncurses +
;; gettext-minimal, whose recipes patch source files in custom phases.
;;
;; SELF-DISCRIMINATING, and discriminating on the PHASES axis specifically:
;;   (a) CONVERGE          — popt (with the phase) lowers to the corpus oracle drv
;;       (store-path-equal ⇒ NAR-hash-equal).
;;   (b) DISCRIMINATE-src  — a perturbed recipe (one wrong byte in the source hash)
;;       lowers to a DIFFERENT drv (the differential can never rot vacuous).
;;   (c) PHASES LOAD-BEARING — the SAME recipe with `phases` STRIPPED lowers to a
;;       DIFFERENT drv: the declared phase is load-bearing, not decorative — a
;;       bridge that dropped it (or generated a different gexp) would diverge from
;;       the oracle.
;;
;; Derivation-level and build-free (`#:graft? #f`). The matching BUILD + `--check`
;; (prime directive 1) + NAR-equality is the rest of the `corpus-popt` gate. Run as
;; a repl SCRIPT so the process exit status is the test result.
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (ice-9 format)
             (srfi srfi-1)
             (json)
             (gnu packages)                 ;the ORACLE (specification->package popt)
             (system td-recipe))

(define (env-json name)
  (let ((v (getenv name)))
    (when (or (not v) (string=? v ""))
      (format #t "FAIL: ~a not set — the corpus-popt gate must pass the emitted \
recipe JSON (tsc -> boa -> recipe()).~%" name)
      (exit 2))
    v))

;; Re-serialize the recipe JSON with its "phases" field removed (leg (c)).
(define (strip-phases json)
  (scm->json-string (alist-delete "phases" (json-string->scm json))))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (drv-name pkg) (derivation-file-name (package-derivation store pkg #:graft? #f)))

  (let* ((recipe-json (env-json "TD_RECIPE_POPT_JSON"))
         (oracle    (drv-name (specification->package "popt")))
         (candidate (drv-name (json-recipe->package recipe-json)))
         (perturbed (drv-name (json-recipe->package
                               (env-json "TD_RECIPE_POPT_PERTURBED_JSON"))))
         (nophases  (drv-name (json-recipe->package (strip-phases recipe-json))))
         (converge?     (string=? oracle candidate))
         (disc-src?     (not (string=? oracle perturbed)))
         (disc-phases?  (not (string=? oracle nophases))))

    (format #t "~%== input-recipes differential: a recipe WITH a custom PHASE (popt) vs. the Guix corpus ==~%")
    (format #t "  oracle    (corpus popt)              : ~a~%" oracle)
    (format #t "  candidate (recipe-popt.ts)           : ~a~%" candidate)
    (format #t "  perturbed (wrong source hash)        : ~a~%" perturbed)
    (format #t "  no-phases (phases stripped)          : ~a~%" nophases)
    (format #t "~%  (a) converge        (candidate == oracle) : ~a~%" converge?)
    (format #t "  (b) discriminate-src (perturbed != oracle) : ~a~%" disc-src?)
    (format #t "  (c) phases load-bearing (no-phases != oracle): ~a~%~%" disc-phases?)

    (cond
     ((not converge?)
      (format #t "FAIL: the TS-authored recipe WITH a custom phase does NOT \
reproduce the corpus oracle's derivation — the reconstructed popt diverges from \
the package it claims to be (the generated modify-phases gexp is not \
byte-identical to the corpus phase).~%")
      (exit 1))
     ((not disc-src?)
      (format #t "FAIL: differential is vacuous — a perturbed recipe (wrong source \
hash) did NOT change the derivation.~%")
      (exit 1))
     ((not disc-phases?)
      (format #t "FAIL: the declared phase is NOT load-bearing — stripping `phases` \
left the derivation unchanged, so the bridge is ignoring phases (or the package \
has none).~%")
      (exit 1))
     (else
      (format #t "PASS: a TS-authored recipe with a custom build phase (popt) \
lowers store-path-identical to the Guix corpus oracle — the phase DATA lowered to \
the byte-identical modify-phases gexp; a perturbed source diverges, and the \
declared phase is load-bearing (stripping it diverges).~%")
      (exit 0)))))
