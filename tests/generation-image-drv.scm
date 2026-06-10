;; tests/generation-image-drv.scm — lower the M10.1 bootc generation images so the
;; Makefile `generation-image` rung can build them, `guix build --check` them, and
;; crack their tarballs. Prints DRV_* lines for the rung to consume.
;;
;;   DRV_GEN1 / DRV_GEN2 — bootc generation images for two distinct generations.
;;       They must differ (each carries its own generation's initrd, which mounts
;;       that generation's distinct root) — the per-generation discriminator.
;;   DRV_BASE — the PLAIN userspace docker image for the same OS (no /boot). The
;;       rung asserts /boot is ABSENT here and PRESENT in the bootc image, so the
;;       "made bootable" claim is self-discriminating.
;;
;; Run as a repl SCRIPT (not piped via STDIN) — see tests/typed-diff.scm.
(use-modules (guix store)
             (guix derivations)
             (guix monads)
             (guix gexp)
             (gnu)
             (gnu system image)
             (ice-9 format)
             (srfi srfi-13)
             (system td-typed)
             (system td-generation))

(with-store store
  ;; Offline contract (triage): no substitutes, no remote offloading (guix repl
  ;; ignores GUIX_BUILD_OPTIONS) — see tests/typed-diff.scm / check.sh.
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (drv obj)
    (derivation-file-name (run-with-store store obj)))

  (define (gen-image n)
    (drv (td-generation-image (td-config #:generation n))))

  ;; P1: a generation image with NO generation id (config root = shared td-root)
  ;; must be rejected at the API boundary. P3: match the SPECIFIC guard error
  ;; ("requires a generation id"), not any exception — an unrelated failure must
  ;; NOT count as the intended rejection.
  (define (rejected-for-no-gen?)
    (catch #t
      (lambda () (td-generation-image (td-config)) #f)
      (lambda args
        (string-contains (object->string args) "requires a generation id"))))
  (format #t "REJECTS_NO_GEN=~a~%" (if (rejected-for-no-gen?) "yes" "no"))

  (define base-userspace
    (drv (lower-object
          (system-image
           (image-with-os
            docker-image
            (td-config->operating-system (td-config #:generation 1)))))))

  (format #t "DRV_GEN1=~a~%" (gen-image 1))
  (format #t "DRV_GEN2=~a~%" (gen-image 2))
  (format #t "DRV_BASE=~a~%" base-userspace))
