;; system/td.scm — the v0 target: a minimal, bootable td system.
;;
;; This is the smallest operating-system declaration that builds into a bootable
;; image (DESIGN.md §2.1). Keep it minimal; later milestones add services and
;; harden them on top of this.
;;
;; It is also the FROZEN differential oracle (DESIGN §2.5): the typed front-end
;; (system td-typed) independently reconstructs this system and the M4/M5/M6
;; differentials prove they converge to the same store paths. As of the M7 sign-off
;; (2026-06-06, §4.3) td ships guix-free by construction, so this oracle was
;; re-baselined to the guix-free system (no guix-service-type, guix-free-marker
;; embedded) — matching the typed default `(td-config)` at its new digest.
(define-module (system td)
  #:use-module (gnu)
  #:use-module (gnu bootloader grub)
  #:use-module (gnu services base)       ;guix-service-type (deleted when guix-free)
  #:use-module (gnu services networking)
  #:use-module (gnu services ssh)
  #:use-module (gnu system file-systems)
  #:use-module (gnu packages containers) ;crun — the container runtime td ships
  #:use-module (gnu packages rust-apps)  ;procs/fd/ripgrep/sd/eza/bat — Rust userland
  ;; guix-free-marker (embedded build gate) + cgroup2-file-system (container host)
  #:use-module (system td-hardening)
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

    ;; The root fs, plus the cgroup2 mount that makes td a container host (M9).
    ;; cgroup2-file-system is shared with the typed compiler so the two cannot
    ;; drift (the differentials would catch it, but sharing prevents it).
    (file-systems
     (cons* (file-system
              (device (file-system-label "td-root"))
              (mount-point "/")
              (type "ext4"))
            cgroup2-file-system
            %base-file-systems))

    ;; Guix-free by construction (M7, shipped default signed off 2026-06-06,
    ;; DESIGN §4.3). td ships an image-swap-only distro: the realized image carries
    ;; no `guix`/`guix-daemon`, so there is no imperative `guix install` surface.
    ;; The `guix-free-marker` is a build-time package whose build FAILS if any
    ;; bin/guix is in %base-packages' closure; because it lives in `packages`,
    ;; EVERY lowering (qcow2, docker, bare) builds it, so a guix-ful regression
    ;; refuses to build rather than silently shipping guix. This mirrors EXACTLY
    ;; what `td-config->operating-system` emits for a `ship-guix? #f` config
    ;; (the typed default), so the M4/M5/M6 differentials converge on this oracle —
    ;; and convergence in turn ENFORCES the marker here (drop it and `make diff`
    ;; reddens). See (system td-hardening).
    ;; M9: ship `crun` in the base — a container host needs a container runtime.
    ;; It joins %base-packages in the system profile (so the booted base has crun
    ;; on PATH), and the guix-free-marker scans this same set (crun pulls in no
    ;; guix, so the marker still passes). The typed compiler ships crun identically.
    ;; rust-userland (human-directed 2026-06-17): ALONGSIDE crun, ship the
    ;; Rust-native userland the pinned channel carries — procs (ps/top), fd
    ;; (find), ripgrep (grep), sd (sed), eza (ls), bat (cat). ADDITIVE: this does
    ;; not remove the GNU tools (a Guix-built closure bakes coreutils/bash into
    ;; activation/shepherd), it ships the Rust tools on PATH beside them. They are
    ;; injected base userland (not swappable manifest content), so the typed
    ;; compiler prepends the IDENTICAL list to %base-packages, in the same order —
    ;; keeping the M4/M5/M6 differentials byte-converged on this oracle. The
    ;; guix-free-marker scans this same set (the Rust apps pull in no guix).
    (packages (let ((pkgs (cons* crun procs fd ripgrep sd eza bat %base-packages)))
                (cons (guix-free-marker pkgs) pkgs)))

    ;; sshd requires 'networking; %base-services provides only 'loopback. dhcpcd
    ;; brings up the VM's QEMU user-mode NIC and provides 'networking — the same
    ;; wiring upstream's own ssh system test uses. `(delete guix-service-type)`
    ;; removes the guix-daemon (the service that pulls guix into the base closure),
    ;; completing the guix-free guarantee the marker above enforces on `packages`.
    ;; `guix-free-privsep-service` restores the sshd privsep dir (/var/empty) that
    ;; guix-service-type used to set up as a side effect (via the build users whose
    ;; home is /var/empty) — without it a guix-free sshd aborts every connection
    ;; ("/var/empty must be owned by root and not group or world-writable"). See
    ;; (system td-hardening).
    (services
     (modify-services
         (cons* (service dhcpcd-service-type)
                (service openssh-service-type td-ssh-configuration)
                guix-free-privsep-service
                %base-services)
       (delete guix-service-type)))))

;; Allow `guix system build system/td.scm` to pick up the declaration.
td-system
