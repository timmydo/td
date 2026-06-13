;; tests/ts-diff.scm — the ts-frontend differential (DESIGN §7.1 acceptance
;; #1/#2). The capstone of Phase 1: a TypeScript spec, transpiled by tsc and
;; evaluated by the boa evaluator (the `ts-diff` rung runs that front-end and
;; passes the emitted config JSON in via the environment), lowers to the SAME
;; system derivation as the frozen system/td.scm oracle — exactly the
;; convergence tests/typed-diff.scm proves for the Guile typed front-end, now
;; driven from the TS surface. The Guile/gexp layer (td-config) stays underneath
;; as the migration lowering target (DESIGN §5): the JSON is mapped to a
;; td-config, lowered via td-config->operating-system, and diffed.
;;
;; SELF-DISCRIMINATING, like typed-diff: it asserts BOTH directions —
;;   (a) CONVERGE     — the v0 TS spec (its values are the td-config defaults)
;;       lowers to the SAME system derivation as the frozen oracle.
;;   (b) DISCRIMINATE — a perturbed TS spec (sshPort 2222) lowers to a DIFFERENT
;;       derivation, so the differential can never silently rot into a vacuous
;;       pass.
;;
;; Run as a repl SCRIPT so the process exit status is the test result (the
;; STDIN-piped trap the diff/test rungs document).
(use-modules (guix store)
             (guix derivations)
             (guix monads)
             (gnu)
             (gnu system)
             (ice-9 format)
             (json)
             (system td)
             (system td-typed))

(define (env-json name)
  (let ((v (getenv name)))
    (when (or (not v) (string=? v ""))
      (format #t "FAIL: ~a not set — the ts-diff rung must pass the emitted \
config JSON (tsc -> boa -> system()).~%" name)
      (exit 2))
    (json-string->scm v)))

;; guile-json's json-string->scm yields an alist with STRING keys.
(define (ref alist key)
  (let ((p (assoc key alist)))
    (unless p
      (format #t "FAIL: the emitted config JSON is missing the field ~a.~%" key)
      (exit 2))
    (cdr p)))

;; guile-json gives exact integers for JSON integers, but normalise defensively
;; so a future inexact number cannot silently change the lowered system.
(define (exact-int x)
  (if (and (number? x) (not (exact? x))) (inexact->exact x) x))

;; Map the TS spec's emitted JSON (camelCase) onto td-config (the kebab-case
;; Guile lowering target). v0 carries only the scalar fields; manifest /
;; generation / persistent-paths are left to default, so the default spec lowers
;; byte-identically to the frozen oracle.
(define (json->td-config a)
  (td-config
   #:host-name             (ref a "hostName")
   #:timezone              (ref a "timezone")
   #:locale                (ref a "locale")
   #:bootloader-target     (ref a "bootloaderTarget")
   #:root-fs-label         (ref a "rootFsLabel")
   #:root-mount            (ref a "rootMount")
   #:root-fs-type          (ref a "rootFsType")
   #:ssh-port              (exact-int (ref a "sshPort"))
   #:ssh-password-auth?    (ref a "sshPasswordAuth")
   #:ssh-challenge-response? (ref a "sshChallengeResponse")
   #:ship-guix?            (ref a "shipGuix")))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (system-drv os)
    (derivation-file-name
     (run-with-store store (operating-system-derivation os))))

  (let* ((oracle    (system-drv td-system))
         (compiled  (system-drv
                     (td-config->operating-system
                      (json->td-config (env-json "TD_TS_DEFAULT_JSON")))))
         (perturbed (system-drv
                     (td-config->operating-system
                      (json->td-config (env-json "TD_TS_PERTURBED_JSON")))))
         (converge?     (string=? oracle compiled))
         (discriminate? (not (string=? oracle perturbed))))

    (format #t "~%== ts-frontend differential: TS spec (tsc -> boa -> config) vs. frozen oracle ==~%")
    (format #t "  oracle (system td)          : ~a~%" oracle)
    (format #t "  compiled (TS v0 spec)       : ~a~%" compiled)
    (format #t "  perturbed (TS sshPort 2222) : ~a~%" perturbed)
    (format #t "~%  (a) converge  (compiled == oracle)      : ~a~%" converge?)
    (format #t "  (b) discriminate (perturbed != oracle)  : ~a~%~%" discriminate?)

    (cond
     ((not converge?)
      (format #t "FAIL: the TS v0 spec does not reproduce the oracle's system \
derivation — the TS surface diverged from the frozen oracle.~%")
      (exit 1))
     ((not discriminate?)
      (format #t "FAIL: differential is vacuous — a perturbed TS spec did NOT \
change the derivation. The differential has lost discriminating power.~%")
      (exit 1))
     (else
      (format #t "PASS: the TS v0 spec lowers store-path-identical to the frozen \
oracle, and a perturbed spec diverges.~%")
      (exit 0)))))
