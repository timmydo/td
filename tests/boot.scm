;; tests/boot.scm — the td system behavioral test (DESIGN.md §2.1, §2.4).
;;
;; Boot the td system in a VM and assert, on a single boot:
;;   - M1: the guest's running kernel release (`uname -r`) equals the version
;;     pinned in the declaration (derived from the declaration, no magic
;;     constant to drift);
;;   - M2: the declared ssh-daemon shepherd unit is running and its port listens;
;;   - M3: default-deny hardening — the daemon refuses password authentication.
(define-module (tests boot)
  #:use-module (gnu tests)
  #:use-module (gnu system)
  #:use-module (gnu system vm)
  #:use-module (gnu services ssh)
  #:use-module (gnu packages ssh)
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
                       (srfi srfi-13)
                       (ice-9 popen)
                       (ice-9 rdelim))

          (define marionette (make-marionette (list #$vm)))

          ;; system-test-runner writes the SRFI-64 log into #$output, so the
          ;; builder produces its output path and the process exit status
          ;; reflects the test result. (The previous `node-test-runner` was an
          ;; unbound variable, so this builder never actually ran — see the
          ;; commit message.)
          (test-runner-current (system-test-runner #$output))
          (test-begin "td-boot")

          ;; M1: the running kernel matches the declaration. Use Guile's
          ;; built-in `uname` (no subprocess / no reliance on the guest PATH).
          (test-equal "running kernel matches the declared kernel"
            #$%expected-kernel-release
            (marionette-eval '(utsname:release (uname)) marionette))

          ;; M2: the declared service is up and its port listens.
          (test-assert "ssh-daemon shepherd unit is running"
            (marionette-eval
             '(begin
                (use-modules (gnu services herd))
                ;; Idempotent: returns the running service (truthy), #f if it
                ;; cannot be brought up.
                (start-service 'ssh-daemon))
             marionette))

          (test-assert "declared sshd port is listening"
            (wait-for-tcp-port
             #$(openssh-configuration-port-number td-ssh-configuration)
             marionette))

          ;; M3: default-deny hardening — the daemon must refuse password
          ;; authentication. We ask the server which methods it will allow by
          ;; offering the "none" method (PreferredAuthentications=none); the
          ;; server replies with the methods that "can continue". This depends
          ;; ONLY on the daemon config (no account, PAM, or credential), so the
          ;; assertion fails iff the hardening is absent — verified by flipping
          ;; password-authentication? in a differential run.
          ;; The server's verbose handshake advertises the methods that "can
          ;; continue". With the hardening this is "publickey" only; without it,
          ;; "publickey,password" (verified by a differential run). We require
          ;; that we saw the advert and that no password-based method is offered.
          (let ((advert
                 (marionette-eval
                  '(begin
                     (use-modules (ice-9 popen) (ice-9 rdelim))
                     (let* ((cmd (string-append
                                  #$(file-append openssh "/bin/ssh")
                                  " -v -o PreferredAuthentications=none"
                                  " -o StrictHostKeyChecking=no"
                                  " -o UserKnownHostsFile=/dev/null"
                                  " -o ConnectTimeout=15"
                                  " probe@localhost true 2>&1"))
                            (port (open-input-pipe cmd))
                            (output (read-string port)))
                       (close-pipe port)
                       output))
                  marionette)))
            (test-assert "daemon denies password authentication (default-deny)"
              (and (string-contains advert "Authentications that can continue")
                   (string-contains advert "publickey")
                   (not (string-contains advert "password"))
                   (not (string-contains advert "keyboard-interactive")))))

          (test-end)
          (exit (zero? (test-runner-fail-count (test-runner-current)))))))

  (gexp->derivation "td-boot-test" test))

(define %test-td-boot
  (system-test
   (name "td-boot")
   (description
    "Boot the td system and assert: the running kernel release matches the \
version pinned in the declaration, the ssh-daemon shepherd unit is running, the \
declared sshd port is listening, and the daemon denies password authentication \
(default-deny hardening).")
   (value (run-td-boot-test))))
