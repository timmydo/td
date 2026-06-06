;; tests/boot.scm — the td system behavioral test (DESIGN.md §2.1, §2.4).
;;
;; Boot the td system in a VM and assert, on a single boot:
;;   - M1: the guest's running kernel release (`uname -r`) equals the version
;;     pinned in the declaration (derived from the declaration, no magic
;;     constant to drift);
;;   - M2: the declared ssh-daemon shepherd unit is running and its port listens;
;;   - M3: default-deny hardening — the daemon refuses password authentication;
;;   - M3+: positive control — a provisioned key-based login actually SUCCEEDS and
;;     we capture the output of a command run over that SSH session. "Denied" and
;;     "works" are independent claims; M3 proved the first, this proves the second.
(define-module (tests boot)
  #:use-module (gnu tests)
  #:use-module (gnu system)
  #:use-module (gnu system shadow)
  #:use-module (gnu system vm)
  #:use-module (gnu system image)
  #:use-module (gnu image)
  #:use-module (gnu services)
  #:use-module (gnu services ssh)
  #:use-module (gnu packages ssh)
  #:use-module (gnu packages virtualization)
  #:use-module ((gnu build marionette) #:select (qemu-command))
  #:use-module (guix gexp)
  #:use-module (guix packages)
  #:use-module (system td)
  #:export (%test-td-boot
            %test-td-disk-boot))

(define %expected-kernel-release
  ;; linux-libre reports `uname -r` as "<version>-gnu".
  (string-append (package-version (operating-system-kernel td-system))
                 "-gnu"))

(define %test-user "tester")

(define %test-os
  ;; A TEST-ONLY overlay on the frozen `td-system`: it adds an unprivileged
  ;; login account and authorizes the committed test public key for it. The
  ;; shipped `td-system` (and the qcow2/OCI images that the M4/M5 differentials
  ;; pin as the oracle) stays untouched — we must not ship this account or key.
  ;; `(inherit config)` on the openssh service preserves the M3 hardening
  ;; (password auth off, root login off), so the positive login below is forced
  ;; through publickey as the non-root %test-user.
  (operating-system
    (inherit td-system)
    (users (cons (user-account
                  (name %test-user)
                  (group "users")
                  (comment "td boot-test login user")
                  (home-directory (string-append "/home/" %test-user)))
                 (operating-system-users td-system)))
    (services
     (modify-services (operating-system-user-services td-system)
       (openssh-service-type config =>
         (openssh-configuration
          (inherit config)
          (authorized-keys
           (list (list %test-user
                       (local-file "keys/td_test_ed25519.pub"))))))))))

(define (run-td-boot-test)
  (define os
    (marionette-operating-system
     %test-os
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

          ;; M3+ positive control: a provisioned key-based login SUCCEEDS and we
          ;; capture the output of a command run over that session. The store
          ;; copy of the private key is 0444 (world-readable) so ssh would refuse
          ;; it ("permissions too open"); copy it out and chmod 0600 first. We
          ;; log in as the non-root %test-user over publickey only (root login
          ;; and password auth are both off per M3), run a small command, and
          ;; assert BOTH the exit status is 0 AND its stdout reached us —
          ;; carrying a sentinel (proves the session/command ran) and the
          ;; username from `id -un` (proves we authenticated AS that user).
          (let ((login
                 (marionette-eval
                  '(begin
                     (use-modules (ice-9 popen) (ice-9 rdelim))
                     (let ((kf "/root/td_test_key"))
                       (copy-file #$(local-file "keys/td_test_ed25519") kf)
                       (chmod kf #o600)
                       (let* ((cmd (string-append
                                    #$(file-append openssh "/bin/ssh")
                                    " -i " kf
                                    " -o IdentitiesOnly=yes"
                                    " -o PreferredAuthentications=publickey"
                                    " -o StrictHostKeyChecking=no"
                                    " -o UserKnownHostsFile=/dev/null"
                                    " -o ConnectTimeout=20"
                                    " -p " (number->string
                                            #$(openssh-configuration-port-number
                                               td-ssh-configuration))
                                    " " #$%test-user "@localhost"
                                    " 'echo TD_LOGIN_OK; id -un' 2>&1"))
                              (port (open-input-pipe cmd))
                              (output (read-string port))
                              (status (close-pipe port)))
                         (list (status:exit-val status) output))))
                  marionette)))
            (test-assert "key-based SSH login succeeds and command output is captured"
              (and (eqv? 0 (car login))
                   (string-contains (cadr login) "TD_LOGIN_OK")
                   (string-contains (cadr login) #$%test-user))))

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

;;;
;;; Disk-image boot test (triage #2): boot the qcow2 through its BOOTLOADER.
;;;
;;; The %test-td-boot above uses `(virtual-machine os)`, which direct-kernel-boots
;;; (qemu -kernel/-initrd) — it never exercises GRUB, the partition table, or the
;;; disk image, so a broken bootloader/disk could still pass. This test boots the
;;; qcow2 DISK image the way `make build` builds and reproducibility-checks it:
;;; SeaBIOS -> GRUB (installed on the image's /dev/vda) -> kernel -> init.
;;;
;;; The booted OS is the shipped `td-system` instrumented ONLY with the marionette
;;; backdoor (so we can drive it) — the same kind of test-only overlay as
;;; %test-os, and crucially the bootloader-configuration, file-systems and image
;;; type are exactly the shipped image's. Reaching a live guest at all proves the
;;; whole disk-boot chain worked; we assert the kernel release as the concrete
;;; oracle, now via the real bootloader path rather than direct-kernel.
;;;
;;; (Residual: this is not byte-identical to the shipped qcow2 — it carries the
;;; backdoor service. A byte-exact boot of the un-instrumented image would need a
;;; serial-console/ssh harness instead of the marionette; noted for follow-up.)

(define (run-td-disk-boot-test)
  (define os
    (marionette-operating-system
     td-system
     #:imported-modules '((gnu services herd))))

  ;; Build the qcow2 exactly as `guix system image -t qcow2` does.
  (define image
    (system-image ((image-type-constructor qcow2-image-type) os)))

  (define test
    (with-imported-modules '((gnu build marionette))
      #~(begin
          (use-modules (gnu build marionette)
                       (srfi srfi-1)
                       (srfi srfi-13)
                       (srfi srfi-64))

          ;; No -kernel/-initrd: boot the disk so firmware -> GRUB runs.
          ;; -snapshot keeps the run ephemeral (writable overlay, discarded).
          (define qemu-cmd
            `(,(string-append #$qemu-minimal "/bin/" #$(qemu-command))
              "-snapshot"
              ,@(if (file-exists? "/dev/kvm") '("-enable-kvm") '())
              "-no-reboot"
              "-m" "512"
              "-drive" ,(string-append "file=" #$image
                                       ",if=virtio,format=qcow2")))

          (define marionette (make-marionette qemu-cmd))

          (test-runner-current (system-test-runner #$output))
          (test-begin "td-disk-boot")

          ;; Permanent guard (triage #5): assert the boot used the DISK/bootloader
          ;; path, not direct-kernel. Without this, a regression back to
          ;; `-kernel`/`-initrd` (or `(virtual-machine os)`) would still satisfy
          ;; the uname assertion below and stay green. We require the qemu command
          ;; to carry the qcow2 disk and to carry NO -kernel/-initrd, so a
          ;; direct-kernel regression reddens here structurally.
          (test-assert "boots from the qcow2 disk via firmware->GRUB (no direct-kernel)"
            (and (not (member "-kernel" qemu-cmd))
                 (not (member "-initrd" qemu-cmd))
                 (any (lambda (a)
                        (and (string? a) (string-contains a "format=qcow2")))
                      qemu-cmd)))

          (test-equal "qcow2 disk boots through GRUB; kernel matches declaration"
            #$%expected-kernel-release
            (marionette-eval '(utsname:release (uname)) marionette))

          (test-end)
          (exit (zero? (test-runner-fail-count (test-runner-current)))))))

  (gexp->derivation "td-disk-boot-test" test))

(define %test-td-disk-boot
  (system-test
   (name "td-disk-boot")
   (description
    "Boot the qcow2 disk image through its GRUB bootloader (not a direct-kernel \
VM) and assert the running kernel matches the declaration — exercising the \
bootloader, partition table and disk image that the direct-kernel boot skips.")
   (value (run-td-disk-boot-test))))
