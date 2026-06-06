;; tests/typed-diff.scm — M4 differential test (DESIGN.md §2.4, §2.5).
;;
;; The oracle is the hand-written (system td) gexp. The candidate is the typed
;; front-end (system td-typed) compiled to an operating-system. This proves the
;; replacement-via-differential-oracle discipline that the whole north star
;; rests on: build the same thing both ways and diff the store paths.
;;
;; It is SELF-DISCRIMINATING — it asserts BOTH directions so the oracle can
;; never silently rot into a vacuous pass (the M3 false-green lesson, promoted
;; to a permanent guardrail):
;;
;;   (a) CONVERGE  — the equivalent typed config (%td-default-config) lowers to
;;       the SAME system derivation as the frozen hand-written td-system.
;;   (b) DISCRIMINATE — a deliberately perturbed config (ssh-port 2222) lowers
;;       to a DIFFERENT derivation. This is the red-run baked into the suite: if
;;       the comparison ever stops distinguishing systems, (b) fails.
;;
;; Run as a script so the process exit status is the test result:
;;   guix ... repl -L . tests/typed-diff.scm     # honors (exit) — unlike a
;;                                                 # script piped via STDIN.
(use-modules (guix store)
             (guix derivations)
             (guix monads)
             (gnu)
             (gnu system)
             (ice-9 format)
             (system td)
             (system td-typed))

(with-store store
  ;; Offline contract (triage): forbid substitutes AND remote offloading for this
  ;; store session. The shared host daemon has network + a nonguix substitute URL
  ;; (check.sh), and `guix repl` does not read GUIX_BUILD_OPTIONS — so set both
  ;; here. This guarantees no binary substitutes and no remote builders; a cold
  ;; fixed-output SOURCE fetch by the shared daemon is still possible (the narrowed
  ;; contract — see check.sh / DESIGN §5).
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  ;; Lower an operating-system to its system derivation and return the .drv
  ;; store path. No building — this is a pure structural fingerprint of the
  ;; system, computed from the derivation graph.
  (define (system-drv os)
    (derivation-file-name
     (run-with-store store (operating-system-derivation os))))

  (let* ((oracle    (system-drv td-system))
         (compiled  (system-drv (td-config->operating-system %td-default-config)))
         (perturbed (system-drv
                     (td-config->operating-system (td-config #:ssh-port 2222))))
         (converge? (string=? oracle compiled))
         (discriminate? (not (string=? oracle perturbed))))

    (format #t "~%== M4 differential: typed front-end vs. hand-written gexp ==~%")
    (format #t "  oracle (system td)        : ~a~%" oracle)
    (format #t "  compiled (default config) : ~a~%" compiled)
    (format #t "  perturbed (ssh-port 2222) : ~a~%" perturbed)
    (format #t "~%  (a) converge  (compiled == oracle)      : ~a~%" converge?)
    (format #t "  (b) discriminate (perturbed != oracle)  : ~a~%~%" discriminate?)

    (cond
     ((not converge?)
      (format #t "FAIL: typed front-end does not reproduce the oracle's system \
derivation.~%")
      (exit 1))
     ((not discriminate?)
      (format #t "FAIL: differential is vacuous — a perturbed config did NOT \
change the derivation. The oracle has lost discriminating power.~%")
      (exit 1))
     (else
      (format #t "PASS: compiled output is store-path-identical to the oracle, \
and the differential distinguishes a changed config.~%")
      (exit 0)))))
