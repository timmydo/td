;; system/td.scm — the v0 target: a minimal, bootable td system.
;;
;; This is the smallest operating-system declaration that builds into a bootable
;; image (DESIGN.md §2.1). Keep it minimal; later milestones add services and
;; harden them on top of this.
(define-module (system td)
  #:use-module (gnu)
  #:use-module (gnu bootloader grub)
  #:use-module (gnu system file-systems)
  #:export (td-system))

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

    (services %base-services)))

;; Allow `guix system build system/td.scm` to pick up the declaration.
td-system
