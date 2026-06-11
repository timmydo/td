;; tests/rollback-drv.scm — lower the M10.3 rollback artifacts so the Makefile
;; `rollback` rung can build them, `guix build --check` them, and validate the
;; tree before booting the disk. Prints DRV_* lines for the rung to consume.
;;
;;   DRV_TREE — the placed target tree (gens 1,2; --mkfs; --boot-label td-boot):
;;       per-generation /boot + LIVE labeled root filesystems + the managed menu.
;;   DRV_DISK — that tree assembled into the raw bootable disk the marionette
;;       test boots twice (system/td-disk.scm).
;;
;; Run as a repl SCRIPT (not piped via STDIN) — see tests/typed-diff.scm.
(use-modules (guix store)
             (guix derivations)
             (guix monads)
             (ice-9 format)
             (tests rollback))

(with-store store
  ;; Offline contract (triage): no substitutes, no remote offloading (guix repl
  ;; ignores GUIX_BUILD_OPTIONS) — see tests/typed-diff.scm / check.sh.
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (drv obj)
    (derivation-file-name (run-with-store store obj)))

  (format #t "DRV_TREE=~a~%" (drv (td-rollback-tree)))
  (format #t "DRV_DISK=~a~%" (drv (td-rollback-disk-value))))
