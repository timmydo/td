;; system/td.scm — the v0 target: a minimal, bootable td system.
;;
;; This is the smallest operating-system declaration that builds into a bootable
;; image (DESIGN.md §2.1). Keep it minimal; later milestones add services and
;; harden them on top of this.
(define-module (system td)
  #:use-module (gnu)
  #:use-module (gnu bootloader grub)
  #:use-module (gnu services networking)
  #:use-module (gnu services ssh)
  #:use-module (gnu system file-systems)
  #:export (td-system
            td-ssh-configuration))

(define td-ssh-configuration
  ;; The v0 network service (DESIGN.md §2.4, milestone 2): OpenSSH. The port is
  ;; declared explicitly so the behavioral test can derive the asserted port
  ;; from this declaration — no magic constant to drift — mirroring how the
  ;; boot test derives the expected kernel release from the declaration.
  ;;
  ;; Milestone 3 (DESIGN.md §2.4): default-deny hardening. Password
  ;; authentication defaults to #t; disabling it makes the daemon key-only, so a
  ;; password login — even with the correct password — is refused. challenge-
  ;; response is pinned off as well so PAM keyboard-interactive cannot smuggle a
  ;; password back in. (permit-root-login already defaults to #f.)
  (openssh-configuration
   (port-number 22)
   (password-authentication? #f)
   (challenge-response-authentication? #f)))

(define td-system
  (operating-system
    (host-name "td")
    (timezone "UTC")
    (locale "en_US.utf8")

    (bootloader
     (bootloader-configuration
      (bootloader grub-bootloader)
      (targets '("/dev/vda"))))

    (file-systems
     (cons (file-system
             (device (file-system-label "td-root"))
             (mount-point "/")
             (type "ext4"))
           %base-file-systems))

    (services
     (cons* ;; sshd requires 'networking; %base-services provides only
            ;; 'loopback. dhcpcd brings up the VM's QEMU user-mode NIC and
            ;; provides 'networking — the same wiring upstream's own ssh system
            ;; test uses.
            (service dhcpcd-service-type)
            (service openssh-service-type td-ssh-configuration)
            %base-services))))

;; Allow `guix system build system/td.scm` to pick up the declaration.
td-system
