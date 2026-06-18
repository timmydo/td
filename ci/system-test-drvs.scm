;; ci/system-test-drvs.scm — lower every marionette system test and every
;; container-rung app artifact to its derivation file name, one per line.
;; Mirrors the Makefile recipes' inline repl expressions (see `rollback`,
;; `container`); used by
;; ci/lower-check-drvs.sh to enumerate the CI store image contents.
;;
;; NOTE (move-off-Guile §5, lever 3): the disk-boot and reset tests were moved to
;; td's OWN SSH-driven VM harness (gates boot-disk-native / reset-native), which
;; have NO (gnu tests) system-test drv — they build a qcow2 image via
;; tests/boot-native-drv.scm / tests/reset-native-drv.scm and need qemu-minimal +
;; openssh + nss-wrapper at run time. Staging those into the CI store image is a
;; ci-image-pipeline follow-up (the full-ci check is not yet a required gate; the
;; dev-machine ./check.sh covers them today).
(use-modules (guix) (gnu tests)
             (tests rollback) (tests container))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (for-each (lambda (t)
              (format #t "~a~%"
                      (derivation-file-name
                       (run-with-store store (system-test-value t)))))
            (list %test-td-rollback %test-td-container))
  (for-each (lambda (m)
              (format #t "~a~%"
                      (derivation-file-name (run-with-store store (m)))))
            (list td-app-image td-app-bundle td-app-badentry-image
                  td-app-badentry-bundle td-app-cgroup-image
                  td-app-cgroup-bundle)))
