;; tests/oci-diff.scm — M5 differential test (DESIGN.md §2.4 step 5, §2.5).
;;
;; M5 pulls the north-star "store path doubles as OCI digest" thread (DESIGN §0):
;; the SAME declaration that boots as a VM image (M1–M4) must also lower to a
;; reproducible Docker/OCI image. This is the derivation-level half of that rung
;; — the cheap, fast-path structural fingerprint. The bit-for-bit reproducibility
;; half lives in the Makefile's `oci` target (`guix build --check` on this very
;; derivation).
;;
;; Same self-discriminating shape as tests/typed-diff.scm (the M3 false-green
;; lesson, kept as a permanent guardrail) — but the artifact under test is the
;; *docker image* derivation, not the system derivation. It asserts BOTH:
;;
;;   (a) CONVERGE   — the typed front-end's default config (%td-default-config)
;;       lowers to the SAME docker-image derivation as the frozen hand-written
;;       td-system oracle. The typed layer drives the OCI artifact too, not just
;;       the bootable system.
;;   (b) DISCRIMINATE — a perturbed config (ssh-port 2222) lowers to a DIFFERENT
;;       docker-image derivation. If the comparison ever stops distinguishing
;;       configs, (b) fails — the red-run baked into the suite.
;;
;; Run as a script so the process exit status is the test result (`guix repl FILE`
;; honors `(exit)`, unlike a script piped via STDIN — see tests/typed-diff.scm).
(use-modules (guix store)
             (guix derivations)
             (guix gexp)
             (guix monads)
             (gnu)
             (gnu system image)
             (ice-9 format)
             (system td)
             (system td-typed))

(with-store store
  ;; Lower an operating-system to the derivation of its Docker/OCI image and
  ;; return the .drv store path. `(image-with-os docker-image os)` is exactly the
  ;; docker image-type's constructor (`guix system image -t docker` builds the
  ;; same thing); `system-image` returns the (lowerable) image derivation, which
  ;; `lower-object` interns. No image is built here — this is a pure structural
  ;; fingerprint of the OCI artifact.
  (define (oci-drv os)
    (derivation-file-name
     (run-with-store store
       (lower-object
        (system-image (image-with-os docker-image os))))))

  (let* ((oracle    (oci-drv td-system))
         (compiled  (oci-drv (td-config->operating-system %td-default-config)))
         (perturbed (oci-drv
                     (td-config->operating-system (td-config #:ssh-port 2222))))
         (converge? (string=? oracle compiled))
         (discriminate? (not (string=? oracle perturbed))))

    (format #t "~%== M5 differential: OCI image, typed front-end vs. hand-written gexp ==~%")
    (format #t "  oracle (system td)        : ~a~%" oracle)
    (format #t "  compiled (default config) : ~a~%" compiled)
    (format #t "  perturbed (ssh-port 2222) : ~a~%" perturbed)
    (format #t "~%  (a) converge  (compiled == oracle)      : ~a~%" converge?)
    (format #t "  (b) discriminate (perturbed != oracle)  : ~a~%~%" discriminate?)

    (cond
     ((not converge?)
      (format #t "FAIL: typed front-end does not reproduce the oracle's OCI image \
derivation.~%")
      (exit 1))
     ((not discriminate?)
      (format #t "FAIL: differential is vacuous — a perturbed config did NOT \
change the OCI image derivation. The oracle has lost discriminating power.~%")
      (exit 1))
     (else
      (format #t "PASS: the OCI image derivation is store-path-identical to the \
oracle, and the differential distinguishes a changed config.~%")
      (exit 0)))))
