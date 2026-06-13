;; tests/corpus-diff.scm — corpus-independence differential (DESIGN §7.1,
;; Phase 2 of the §5 move-off-Guile goal).
;;
;; The oracle is the pinned Guix corpus's own `hello` (§2.5 / prime directive 4):
;; the Guix package collection is the thing td's recipe is being checked AGAINST,
;; the one place `(gnu packages …)` is imported here. The candidate is td's OWN
;; recipe (system td-corpus), reconstructed from upstream coordinates without a
;; corpus lookup. This proves the replacement-via-differential discipline for the
;; CORPUS axis, exactly as tests/typed-diff.scm proves it for the SURFACE axis:
;; lower the same package both ways and diff the derivations.
;;
;; Derivation-level and build-free: compared with `#:graft? #f` so computing the
;; fingerprint never realises an output (grafting would BUILD), keeping this a
;; sub-second structural rung that fails fast. The matching BUILD + `--check`
;; (prime directive 1: a td-recipe-built package must be reproducible) is the
;; heavy `corpus` rung.
;;
;; SELF-DISCRIMINATING — asserts BOTH directions so the differential can never rot
;; into a vacuous pass (the M3 false-green lesson, a permanent guardrail):
;;
;;   (a) DISTINCT     — td-hello is a genuinely separate package object, not the
;;                      corpus `hello` re-exported: `(not (eq? td-hello hello))`.
;;                      Without this, convergence would be trivially true.
;;   (b) CONVERGE     — td's recipe lowers to the SAME derivation as the corpus
;;                      oracle (store-path-equal ⇒ NAR-hash-equal).
;;   (c) DISCRIMINATE — a perturbed recipe (one wrong byte in the upstream source
;;                      hash) lowers to a DIFFERENT derivation. This is the red-run
;;                      baked into the suite: if the diff ever stops distinguishing
;;                      recipes, (c) fails. It also proves the convergence in (b) is
;;                      LOAD-BEARING on td declaring the correct upstream coordinate.
;;
;; Run as a script so the process exit status is the test result (honors (exit) —
;; unlike a script piped via STDIN):
;;   guix … repl -L . tests/corpus-diff.scm
(use-modules (guix store)
             (guix packages)
             (guix derivations)
             (ice-9 format)
             (gnu packages base)             ;the ORACLE — the only corpus import
             (system td-corpus))

(with-store store
  ;; Offline contract (as tests/typed-diff.scm): no substitutes, no remote
  ;; offload. A cold fixed-output SOURCE fetch by the shared daemon is still
  ;; permitted (the narrowed contract); but `#:graft? #f` means we never even
  ;; realise a build here.
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  ;; Ungrafted derivation fingerprint — pure structure, no build.
  (define (pkg-drv pkg)
    (derivation-file-name (package-derivation store pkg #:graft? #f)))

  ;; A perturbed recipe: flip the first base32 digit of the upstream source hash
  ;; (1aqq… -> 1bqq…). Same package shape, a different declared upstream — so the
  ;; build derivation must differ. Build-free: derivation-file-name never fetches.
  (define perturbed-hash
    (string-append "1b" (substring %hello-source-sha256 2)))

  (let* ((oracle    (pkg-drv hello))
         (candidate (pkg-drv td-hello))
         (perturbed (pkg-drv (td-hello/source-sha256 perturbed-hash)))
         (distinct?      (not (eq? td-hello hello)))
         (converge?      (string=? oracle candidate))
         (discriminate?  (not (string=? oracle perturbed))))

    (format #t "~%== corpus-independence differential: td recipe vs. Guix corpus ==~%")
    (format #t "  oracle    (gnu packages base hello) : ~a~%" oracle)
    (format #t "  candidate (system td-corpus td-hello): ~a~%" candidate)
    (format #t "  perturbed (wrong source hash)        : ~a~%" perturbed)
    (format #t "~%  (a) distinct      (td-hello not eq? hello) : ~a~%" distinct?)
    (format #t "  (b) converge      (candidate == oracle)    : ~a~%" converge?)
    (format #t "  (c) discriminate  (perturbed != oracle)    : ~a~%~%" discriminate?)

    (cond
     ((not distinct?)
      (format #t "FAIL: td-hello IS the corpus hello object — convergence would be \
vacuous. The recipe must be authored independently, not re-exported.~%")
      (exit 1))
     ((not converge?)
      (format #t "FAIL: td's own recipe does NOT reproduce the corpus oracle's \
derivation. The reconstructed recipe diverges from the package it claims to be.~%")
      (exit 1))
     ((not discriminate?)
      (format #t "FAIL: differential is vacuous — a perturbed recipe (wrong source \
hash) did NOT change the derivation. The diff has lost discriminating power.~%")
      (exit 1))
     (else
      (format #t "PASS: td's independently-authored recipe is store-path-identical \
to the Guix corpus oracle, and the differential distinguishes a perturbed recipe.~%")
      (exit 0)))))
