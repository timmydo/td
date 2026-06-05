;; tests/boot.scm — the v0 acceptance test (DESIGN.md §2.1).
;;
;; Boot the td system in a VM and assert that the guest's running kernel
;; release (`uname -r`) equals the version of the kernel pinned in the
;; declaration. The expected value is derived from the declaration itself, so
;; the assertion stays honest as the pinned channel moves — there is no magic
;; constant to drift out of sync.
(define-module (tests boot)
  #:use-module (gnu tests)
  #:use-module (gnu system)
  #:use-module (gnu system vm)
  #:use-module (guix gexp)
  #:use-module (guix packages)
  #:use-module (system td)
  #:export (%test-td-boot))

(define %expected-kernel-release
  ;; linux-libre reports `uname -r` as "<version>-gnu".
  (string-append (package-version (operating-system-kernel td-system))
                 "-gnu"))

(define (run-td-boot-test)
  (define os
    (marionette-operating-system
     td-system
     #:imported-modules '((gnu services herd))))

  (define vm (virtual-machine os))

  (define test
    (with-imported-modules '((gnu build marionette))
      #~(begin
          (use-modules (gnu build marionette)
                       (srfi srfi-64)
                       (ice-9 popen)
                       (ice-9 rdelim))

          (define marionette (make-marionette (list #$vm)))

          (test-runner-current (node-test-runner "td-boot"))
          (test-begin "td-boot")

          (test-equal "uname -r matches the declared kernel"
            #$%expected-kernel-release
            (marionette-eval
             '(let* ((port (open-input-pipe "uname -r"))
                     (line (read-line port)))
                (close-pipe port)
                line)
             marionette))

          (test-end)
          (exit (zero? (test-runner-fail-count (test-runner-current)))))))

  (gexp->derivation "td-boot-test" test))

(define %test-td-boot
  (system-test
   (name "td-boot")
   (description
    "Boot the td system and assert the running kernel release matches the \
version pinned in the declaration.")
   (value (run-td-boot-test))))
