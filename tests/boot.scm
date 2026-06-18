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
  #:export (%test-td-disk-boot
            %instrumented-disk-os))

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
     (cons
      ;; The disk-boot test boots a STANDALONE qcow2 (no shared host store —
      ;; unlike the old direct-kernel `(virtual-machine os)`), so the test
      ;; private key's /gnu/store path is ABSENT in the guest. Bake it into the
      ;; image at /td_test_key (0600, ssh-usable) at activation so the
      ;; in-guest ssh client (the M3+ key-login positive control) can use it.
      (simple-service 'td-test-privkey activation-service-type
        #~(begin
            (copy-file #$(local-file "keys/td_test_ed25519") "/td_test_key")
            (chmod "/td_test_key" #o600)))
      (modify-services (operating-system-user-services td-system)
        (openssh-service-type config =>
          (openssh-configuration
           (inherit config)
           (authorized-keys
            (list (list %test-user
                        (local-file "keys/td_test_ed25519.pub")))))))))))

;;;
;;; Disk-image boot test: boot the qcow2 through its BOOTLOADER, then run the
;;; full behavioral assertion suite (M1 kernel, M2 sshd, M3 default-deny, M3+
;;; key-login, M9 container-host) ON that realistic boot.
;;;
;;; This is the SOLE boot test. The former `%test-td-boot` direct-kernel boot
;;; (`(virtual-machine os)` — qemu -kernel/-initrd, which never exercises GRUB,
;;; the partition table, or the disk image, and is NOT how td ships) was removed
;;; (track amortize-vm-boots): its behavioral asserts moved HERE so they are now
;;; verified on the real firmware->GRUB->kernel->init path, eliminating one full
;;; VM boot per check. This test boots the qcow2 DISK image the way `make build`
;;; builds and reproducibility-checks it: SeaBIOS -> GRUB (installed on the
;;; image's /dev/vda) -> kernel -> init.
;;;
;;; The booted OS is %instrumented-disk-os = %test-os (the shipped `td-system`
;;; plus the marionette backdoor AND the test SSH user/key needed by the M3+
;;; key-login positive control); the bootloader-configuration, file-systems and
;;; image type are exactly the shipped image's. A structural guard below asserts
;;; the boot used the disk/GRUB path (no -kernel/-initrd) so a regression to
;;; direct-kernel reddens here rather than passing silently.
;;;
;;; (Residual: this is not byte-identical to the shipped qcow2 — it carries the
;;; backdoor service + the test user. A byte-exact boot of the un-instrumented
;;; image would need a serial-console/ssh harness instead of the marionette;
;;; noted for follow-up.)

;; The shipped td-system as a TEST image: instrumented with the marionette
;; backdoor AND carrying the test SSH user/key (%test-os) so the key-based login
;; positive control can authenticate. Its qcow2 is built exactly as `guix system
;; image -t qcow2` does. The os is module-level and exported so the `reset` rung
;; (tests reset) instruments the SAME system (it derives a non-volatile image
;; variant from it); the extra account is immutable system state, inert to
;; reset's ephemerality assertions. The SHIPPED td-system/qcow2 oracle (the M4/M5
;; differentials) is a DIFFERENT, un-overlaid object and stays untouched.
(define %instrumented-disk-os
  (marionette-operating-system
   %test-os
   #:imported-modules '((gnu services herd))))

(define %instrumented-disk-image
  (system-image ((image-type-constructor qcow2-image-type)
                 %instrumented-disk-os)))

(define (run-td-disk-boot-test)
  (define image %instrumented-disk-image)

  (define test
    (with-imported-modules '((gnu build marionette))
      #~(begin
          (use-modules (gnu build marionette)
                       (srfi srfi-1)
                       (srfi srfi-13)
                       (srfi srfi-64)
                       (ice-9 popen)
                       (ice-9 rdelim))

          ;; No -kernel/-initrd: boot the disk so firmware -> GRUB runs.
          ;; -snapshot keeps the run ephemeral (writable overlay, discarded).
          (define qemu-cmd
            `(,(string-append #$qemu-minimal "/bin/" #$(qemu-command))
              "-snapshot"
              ,@(if (file-exists? "/dev/kvm") '("-enable-kvm") '())
              "-no-reboot"
              ;; Headroom for sshd + an in-guest ssh client (the behavioral
              ;; asserts moved here from the former direct-kernel boot test).
              "-m" "1024"
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

          ;; The behavioral asserts below were the direct-kernel `%test-td-boot`
          ;; test; they now run on THIS realistic firmware->GRUB boot (one fewer
          ;; VM boot per check — see plan/amortize-vm-boots.md). The booted OS is
          ;; %test-os (td-system + the test SSH user/key) so the key-login
          ;; positive control can authenticate.

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
          ;; capture the output of a command run over that session. The private
          ;; key is baked into THIS image at /td_test_key (0600) by %test-os's
          ;; activation service — the standalone disk guest has no
          ;; shared host store to copy it out of (the direct-kernel VM did). We
          ;; log in as the non-root %test-user over publickey only (root login
          ;; and password auth are both off per M3), run a small command, and
          ;; assert BOTH the exit status is 0 AND its stdout reached us —
          ;; carrying a sentinel (proves the session/command ran) and the
          ;; username from `id -un` (proves we authenticated AS that user).
          (let ((login
                 (marionette-eval
                  '(begin
                     (use-modules (ice-9 popen) (ice-9 rdelim))
                     (let ((kf "/td_test_key"))
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

          ;; M9: the booted base is a container host. Two independent claims, each
          ;; a base change verified HERE: (a) cgroup2 is actually mounted at
          ;; /sys/fs/cgroup — this proves the DECLARATIVE cgroup2-file-system mounts
          ;; at boot (the feasibility gate only proved a manual mount), which an OCI
          ;; runtime's startup probe requires; (b) crun is shipped in the system
          ;; profile. The strings are checked in the builder (srfi-13), so the guest
          ;; form only shells out / stats.
          (let ((cgroup-type
                 (marionette-eval
                  '(begin
                     (use-modules (ice-9 popen) (ice-9 rdelim))
                     (let* ((p (open-input-pipe "stat -f -c %T /sys/fs/cgroup"))
                            (o (read-string p)))
                       (close-pipe p)
                       o))
                  marionette))
                (crun?
                 (marionette-eval
                  '(file-exists? "/run/current-system/profile/bin/crun")
                  marionette)))
            (test-assert "base is a container host: cgroup2 mounted and crun shipped"
              (and (string-contains cgroup-type "cgroup2fs")
                   crun?)))

          ;; rust-userland (2026-06-17): the Rust-native base userland is shipped
          ;; AND actually runs. For each tool we check it is on PATH in the system
          ;; profile and that invoking it (`--version`) exits 0 with non-empty
          ;; output IN THE GUEST. This is the DURABLE behavioral leg — it holds
          ;; with no Guix oracle in the room (the tool does its job: it executes),
          ;; unlike the differential gates that only assert td==oracle store paths.
          ;; One guest round-trip runs all six and returns per-tool (bin present?
          ;; exit0? out-len); the builder (srfi-1 `every`) asserts every leg. The
          ;; command name differs from the package for ripgrep (binary `rg`).
          (let ((ran
                 (marionette-eval
                  '(begin
                     (use-modules (ice-9 popen) (ice-9 rdelim))
                     (map (lambda (bin)
                            (let* ((path (string-append
                                          "/run/current-system/profile/bin/" bin))
                                   (p (open-input-pipe
                                       (string-append path " --version 2>&1")))
                                   (out (read-string p))
                                   (st (close-pipe p)))
                              (list bin
                                    (file-exists? path)
                                    (eqv? 0 (status:exit-val st))
                                    (string-length out))))
                          '("procs" "fd" "rg" "sd" "eza" "bat")))
                  marionette)))
            (for-each
             (lambda (r)
               (format #t "    rust userland ~a: present=~a exit0=~a out-len=~a~%"
                       (car r) (cadr r) (caddr r) (cadddr r)))
             ran)
            (test-assert "rust userland shipped and runs (procs/fd/rg/sd/eza/bat --version exits 0)"
              (every (lambda (r) (and (cadr r) (caddr r) (> (cadddr r) 0))) ran)))

          (test-end)
          (exit (zero? (test-runner-fail-count (test-runner-current)))))))

  (gexp->derivation "td-disk-boot-test" test))

(define %test-td-disk-boot
  (system-test
   (name "td-disk-boot")
   (description
    "Boot the qcow2 disk image through its GRUB bootloader (not a direct-kernel \
VM) — exercising the bootloader, partition table and disk image — then assert \
the full behavioral suite on that realistic boot: the running kernel matches the \
declaration, the ssh-daemon unit is up and its port listens, the daemon denies \
password authentication (default-deny), a key-based login succeeds, and the base \
is a container host (cgroup2 mounted, crun shipped).")
   (value (run-td-disk-boot-test))))
