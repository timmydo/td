;; system/td-place.scm — M10.2/M10.3: wire the guix-free PLACER into reproducible,
;; behavioral test derivations.
;;
;; The placer itself (system/td-place.sh) is the deliverable: a POSIX shell tool
;; that runs ON THE TARGET (which has no guix) to extract /boot from a bootc
;; generation image, apply the userspace layers as that generation's own root
;; (optionally onto a live, labeled ext4 image — --mkfs, M10.3), write a
;; per-generation GRUB menu that actually boots (bare-label root= + gnu.system/gnu.load
;; from the image identity), and prune old generations (M10-design.md step 3,
;; "Place"). This module exercises it the way the loop demands — hermetically and
;; reproducibly:
;;
;;   * `td-placed-tree` builds the per-generation bootc images with Guix (the
;;     M10.1 oracle), then runs the placer over them inside a derivation whose
;;     builder PATH contains ONLY base tools — NO guix. A successful build
;;     therefore proves the placer is guix-free BY CONSTRUCTION (guix is not in
;;     the build sandbox at all — the same "absent, so it cannot be used"
;;     guarantee `make no-guix` makes for the shipped image), and the resulting
;;     target tree is itself a reproducible artifact (`guix build --check`).
;;     With #:mkfs? the placer also runs mke2fs (under fakeroot, exactly like
;;     Guix's own image builder) to produce each generation's labeled root.img —
;;     still no guix on PATH.
;;
;; The deployment side is tested against BEHAVIOR, not diffed against a Guix
;; component it does not have (M10-design.md decision 2): tests/place-check.scm
;; cracks the produced tree and asserts placement, the per-generation menu, root
;; selection, preamble preservation, pruning, and (mkfs trees) the ext4
;; label/UUID straight from the superblock.
(define-module (system td-place)
  #:use-module (gnu packages base)        ;tar, coreutils, sed, grep
  #:use-module (gnu packages bash)        ;bash
  #:use-module (gnu packages compression) ;gzip
  #:use-module (gnu packages linux)       ;e2fsprogs, fakeroot (for --mkfs)
  #:use-module (guix gexp)
  #:use-module (guix monads)
  #:use-module (guix store)
  #:use-module (system td-typed)
  #:use-module (system td-generation)
  #:use-module (system td-verity)         ;veritysetup-static (--mkfs, M11)
  #:export (td-placed-tree))

;; Build a target tree by PLACING each generation in GENS (in order, into the
;; same target) with the guix-free placer, pruning to the newest KEEP. The output
;; is the resulting target tree:
;;   boot/td/gen-N/{bzImage,initrd.cpio.gz,td-identity,
;;                  root-label,system,root-uuid,kernel-args}    per kept gen
;;   roots/td/gen-N/root.tar   — that generation's applied userspace root CONTENT
;;   roots/td/gen-N/root.img   — (#:mkfs? only) that content as a live ext4
;;                               filesystem labeled td-root-gen-N, deterministic
;;                               UUID from the image identity
;;   boot/td/boot-label        — (#:boot-label only) the GRUB search label
;;   boot/grub/grub.cfg        — a user preamble the placer must preserve + the
;;                               placer's marker-delimited managed block
;; TRANSFORM-OS is passed through to td-generation-image (the rollback test
;; instruments the OS with the marionette backdoor; default identity).
;; Returns a monadic derivation (suitable for `run-with-store`).
(define* (td-placed-tree #:key (gens '(1 2)) (keep 10)
                         (mkfs? #f) (boot-label #f) (extra-kernel-args #f)
                         (transform-os identity))
  (mlet %store-monad
      ((images (mapm %store-monad
                     (lambda (n)
                       (td-generation-image (td-config #:generation n)
                                            #:transform-os transform-os))
                     gens)))
    (let* ((labels (map (lambda (n)
                          (td-config-effective-root-label (td-config #:generation n)))
                        gens))
           ;; One (gen-string image-path root-label) job per generation. `#$img`
           ;; lowers the bootc image derivation to its output path.
           (jobs   (map (lambda (n img label)
                          #~(list #$(number->string n) #$img #$label))
                        gens images labels)))
      (gexp->derivation (if mkfs? "td-placed-tree-mkfs" "td-placed-tree")
        (with-imported-modules '((guix build utils))
          #~(begin
              (use-modules (guix build utils) (ice-9 match))

              (define placer #$(local-file "td-place.sh"))
              (define sh #$(file-append bash "/bin/bash"))

              ;; Guix-free by construction: ONLY base tools on PATH, no guix.
              ;; e2fsprogs + fakeroot + veritysetup appear ONLY for --mkfs
              ;; (mke2fs run fakerooted exactly as Guix's own make-ext-image
              ;; runs it; veritysetup for the M11 appended dm-verity hash
              ;; tree) — still nothing guix-shaped.
              (setenv "PATH"
                      (string-append #$(file-append coreutils "/bin") ":"
                                     #$(file-append tar "/bin") ":"
                                     #$(file-append gzip "/bin") ":"
                                     #$(file-append sed "/bin") ":"
                                     #$(file-append grep "/bin")
                                     #$@(if mkfs?
                                            (list ":"
                                                  (file-append e2fsprogs "/sbin")
                                                  ":"
                                                  (file-append fakeroot "/bin")
                                                  ":"
                                                  (file-append veritysetup-static
                                                               "/sbin"))
                                            '())))

              (define target   (string-append (getcwd) "/target"))
              (define boot      (string-append target "/boot"))
              ;; The per-generation root CONTENT (applied userspace layers) lands
              ;; here — separate from /boot, as on a real target — so the menu's
              ;; root=td-root-gen-N (bare-label spec) refers to a root that actually exists.
              (define roots     (string-append target "/roots"))
              (define grub-cfg  (string-append boot "/grub/grub.cfg"))
              (mkdir-p (string-append boot "/grub"))
              (mkdir-p (string-append roots "/td"))

              ;; Seed grub.cfg with a user preamble the placer MUST preserve — it
              ;; may only ever touch its own marker-delimited managed block.
              (call-with-output-file grub-cfg
                (lambda (p)
                  (display "\
# td target grub.cfg — user preamble (must be preserved by td-place)
set timeout=5
" p)))

              ;; Place each generation in order into the same target. For mkfs
              ;; trees the WHOLE placer runs under one fakeroot session so the
              ;; extracted rootfs and the mke2fs that copies it agree on root
              ;; ownership (the same reason Guix wraps mke2fs in fakeroot).
              (for-each
               (match-lambda
                 ((gen img label)
                  (apply invoke
                         `(,@(if #$mkfs? '("fakeroot") '())
                           ,sh ,placer
                           "--image"      ,img
                           "--generation" ,gen
                           "--root-label" ,label
                           "--boot-dir"   ,boot
                           "--root-store" ,roots
                           "--grub-cfg"   ,grub-cfg
                           "--keep"       #$(number->string keep)
                           ,@(if #$mkfs? '("--mkfs") '())
                           ,@(let ((bl #$boot-label))
                               (if bl (list "--boot-label" bl) '()))
                           ,@(let ((ka #$extra-kernel-args))
                               (if ka (list "--extra-kernel-args" ka) '()))))))
               (list #$@jobs))

              (copy-recursively target #$output)))))))
