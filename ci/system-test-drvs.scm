;; ci/system-test-drvs.scm — lower every marionette system test and every
;; container-rung app artifact to its derivation file name, one per line.
;; Mirrors the Makefile recipes' inline repl expressions (see `test`,
;; `boot-disk`, `reset`, `rollback`, `container`); used by
;; ci/lower-check-drvs.sh to enumerate the CI store image contents.
(use-modules (guix) (gnu tests)
             (tests boot) (tests reset) (tests rollback) (tests container))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (for-each (lambda (t)
              (format #t "~a~%"
                      (derivation-file-name
                       (run-with-store store (system-test-value t)))))
            (list %test-td-boot %test-td-disk-boot %test-td-reset
                  %test-td-rollback %test-td-container))
  (for-each (lambda (m)
              (format #t "~a~%"
                      (derivation-file-name (run-with-store store (m)))))
            (list td-app-image td-app-bundle td-app-badentry-image
                  td-app-badentry-bundle td-app-cgroup-image
                  td-app-cgroup-bundle)))
