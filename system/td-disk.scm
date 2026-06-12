;; system/td-disk.scm — M10.3: assemble a placed target tree into a bootable disk.
;;
;; The placer (system/td-place.sh) leaves a target in the state a real machine
;; would be in after `td-place`: /boot with per-generation kernels/initrds and the
;; managed GRUB menu, and per-generation LIVE root filesystems (root.img, ext4,
;; labeled). This module turns that tree into the disk the rollback test boots:
;;
;;   MBR partition table (1 MiB gap for GRUB's core.img)
;;     p1  ext4 "td-boot"  bootable — the placed tree's boot/ + the GRUB modules
;;         grub-install would have copied (grub/i386-pc); GRUB's prefix is
;;         (hd0,msdos1)/grub, so the menu's GRUB-root-relative /td/gen-N/...
;;         paths resolve here
;;     p2+ one partition PER GENERATION — the placer's root.img files VERBATIM
;;         (label td-root-gen-N, identity UUID; the menu's bare-label root= target)
;;     pN  ext4 "td-state" — the §2.6 state model's ONE writable filesystem,
;;         created by this harness (the placer never touches it): tier roots
;;         state/ + cache/ + home/ and the default allowlist's backing dirs are
;;         pre-created; everything else (the SSH host key, sentinels) is written
;;         by the GUEST at runtime — that is the point of the partition
;;
;; GRUB is installed image-side exactly like Guix's own `install-grub-disk-image`
;; (grub-mkimage core with biosdisk/part_msdos/ext2 + grub-bios-setup into the
;; MBR/gap) — the same mke2fs/genimage/grub-bios-setup pipeline td's qcow2 `build`
;; rung already proves reproducible with `guix build --check`; this disk gets the
;; same oracle in the `rollback` rung.
;;
;; NOTE (scope): this assembly is TEST FIXTURE construction — it simulates the
;; target disk a real deployment would already have. The guix-free deliverable is
;; the placer; the disk builder may freely use Guix-packaged tools.
(define-module (system td-disk)
  #:use-module (gnu packages base)         ;coreutils, findutils
  #:use-module (gnu packages bootloaders)  ;grub
  #:use-module (gnu packages genimage)
  #:use-module (gnu packages linux)        ;e2fsprogs, fakeroot
  #:use-module (guix gexp)
  #:use-module (guix monads)
  #:use-module (guix store)
  #:use-module (system td-typed)           ;%td-state-label (§2.6)
  #:export (td-rollback-disk
            %td-boot-label))

;; The boot partition's filesystem label — what the managed block's
;; `search --label` selects. The placer is told this via --boot-label.
(define %td-boot-label "td-boot")

;; Fixed, arbitrary UUIDs for the boot and state partition filesystems
;; (determinism only; nothing selects by UUID — boot is found by GRUB's search
;; label, td-state by its label, the roots by their identity-derived UUIDs).
(define %td-boot-uuid "1d000000-0000-4000-8000-00000000b007")
(define %td-state-uuid "1d000000-0000-4000-8000-000000057a7e")

;; Assemble TREE (a lowerable placed-tree built with #:mkfs? #t and
;; #:boot-label %td-boot-label) into a raw bootable disk image carrying the
;; generations in GENS. Returns a monadic derivation.
(define* (td-rollback-disk tree #:key (gens '(1 2)))
  (gexp->derivation "td-rollback-disk"
    (with-imported-modules '((guix build utils))
      #~(begin
          (use-modules (guix build utils)
                       (ice-9 format)
                       (ice-9 popen)
                       (ice-9 rdelim)
                       (srfi srfi-1))

          (define grub #$grub)
          (setenv "PATH"
                  (string-append #$(file-append coreutils "/bin") ":"
                                 #$(file-append e2fsprogs "/sbin") ":"
                                 #$(file-append fakeroot "/bin") ":"
                                 #$(file-append genimage "/bin") ":"
                                 (string-append grub "/bin") ":"
                                 (string-append grub "/sbin")))

          ;; --- 1. Boot partition root: the placed boot/ + GRUB's modules. ---
          ;; copy-recursively keeps the placed layout (grub/grub.cfg, td/...);
          ;; the modules land in grub/i386-pc — what grub-install's disk-image
          ;; branch would have copied, and what core.img's prefix will load
          ;; `normal`/`search`/`test` from.
          (define bootroot (string-append (getcwd) "/bootroot"))
          (mkdir-p bootroot)
          (copy-recursively (string-append #$tree "/boot") bootroot
                            #:keep-permissions? #f)
          (copy-recursively (string-append grub "/lib/grub")
                            (string-append bootroot "/grub")
                            #:keep-permissions? #f)

          ;; --- 2. Boot partition image (same invocation as the placer's mkfs
          ;; and Guix's make-ext-image — the proven-deterministic recipe). ---
          (define (du-kb dir)
            ;; Size from CONTENT, not block accounting: st_blocks is
            ;; filesystem-dependent even settled (btrfs inlines small
            ;; files; ext4 charges extent metadata) — proven live by the
            ;; hosted CI's cross-host --check (same defect class as the
            ;; placer's; see td-place.sh). Apparent bytes + 4KiB per
            ;; entry (inode + dirent headroom) depend only on the tree.
            (let ((entries (find-files dir (const #t) #:directories? #t)))
              (quotient (+ (fold + 0
                                 (map (lambda (f)
                                        (let ((st (lstat f)))
                                          (if (eq? 'regular (stat:type st))
                                              (stat:size st)
                                              0)))
                                      entries))
                           (* 4096 (+ 1 (length entries))))
                        1024)))
          ;; Determinism: copy-recursively re-stamps everything "now", and
          ;; mke2fs stamps the superblock/journal with the current time and a
          ;; random hash seed unless pinned (the placer learned this the
          ;; --check-red way; same medicine here).
          (define (normalize-mtimes! dir)
            (for-each (lambda (f) (utime f 1 1))
                      (find-files dir (const #t) #:directories? #t))
            (utime dir 1 1))
          (define* (mkfs-ext4 dir out label uuid #:optional size-kb)
            (let* ((kb (or size-kb
                           (let ((kb (du-kb dir)))
                             (+ kb (quotient kb 4) 1024)))))
              (setenv "SOURCE_DATE_EPOCH" "1")
              (setenv "E2FSPROGS_FAKE_TIME" "1")
              (invoke "fakeroot" "mke2fs" "-t" "ext4" "-d" dir
                      "-L" label "-U" uuid
                      "-E" (string-append
                            "root_owner=0:0,lazy_itable_init=1,"
                            "lazy_journal_init=1,hash_seed=" uuid)
                      out (format #f "~ak" kb))))
          (normalize-mtimes! bootroot)
          (mkfs-ext4 bootroot "boot.img" #$%td-boot-label #$%td-boot-uuid)

          ;; --- 2b. The td-state partition (§2.6): the ONE writable filesystem.
          ;; The harness creates it — tier roots + the default allowlist's
          ;; backing dirs pre-made, content written by the GUEST at runtime —
          ;; with a fixed 64 MiB size (du-sizing an empty tree would leave no
          ;; room to live in).
          (define stateroot (string-append (getcwd) "/stateroot"))
          (for-each (lambda (d) (mkdir-p (string-append stateroot d)))
                    '("/state/var/lib/ssh" "/cache" "/home"))
          (normalize-mtimes! stateroot)
          (mkfs-ext4 stateroot "state.img" #$%td-state-label #$%td-state-uuid
                     65536)

          ;; --- 3. genimage: MBR disk; p1 boot (1 MiB offset = GRUB gap),
          ;; then one partition per generation, the placer's root.img verbatim. -
          (define root-imgs
            (map (lambda (n)
                   (string-append #$tree "/roots/td/gen-"
                                  (number->string n) "/root.img"))
                 '#$gens))
          (for-each (lambda (f)
                      (unless (file-exists? f)
                        (error "placed tree lacks a live root filesystem" f)))
                    root-imgs)
          (call-with-output-file "genimage.cfg"
            (lambda (port)
              (format port "image image {~%  hdimage {~%    partition-table-type = \"mbr\"~%  }~%")
              (format port "  partition boot {~%    partition-type = 0x83~%    image = \"~a\"~%    offset = \"1048576\"~%    bootable = \"true\"~%  }~%"
                      (string-append (getcwd) "/boot.img"))
              (for-each
               (lambda (n img)
                 (format port "  partition gen~aroot {~%    partition-type = 0x83~%    image = \"~a\"~%    bootable = \"false\"~%  }~%"
                         n img))
               '#$gens root-imgs)
              (format port "  partition tdstate {~%    partition-type = 0x83~%    image = \"~a\"~%    bootable = \"false\"~%  }~%"
                      (string-append (getcwd) "/state.img"))
              (format port "}~%")))
          (mkdir "root")                ;genimage insists on a root path
          (invoke "genimage" "--config" "genimage.cfg")

          ;; --- 4. Install GRUB on the image, as install-grub-disk-image does:
          ;; a minimal core.img in the MBR gap whose prefix is the boot
          ;; partition's /grub. ---
          (let ((image "images/image"))
            (invoke "grub-mkimage" "-O" "i386-pc" "-o" "core.img"
                    "-p" "(hd0,msdos1)/grub"
                    "biosdisk" "part_msdos" "ext2")
            (call-with-output-file "device.map"
              (lambda (port)
                (format port "(hd0) ~a~%" image)))
            (copy-file (string-append grub "/lib/grub/i386-pc/boot.img")
                       "boot-mbr.img")
            ;; grub-bios-setup expects boot.img/core.img under -d DIR with
            ;; those exact names; boot.img is taken — use a private dir.
            (mkdir "grub-setup")
            (copy-file "boot-mbr.img" "grub-setup/boot.img")
            (copy-file "core.img" "grub-setup/core.img")
            (invoke "grub-bios-setup" "-m" "device.map" "-r" "hd0,msdos1"
                    "-d" "grub-setup" image)
            (copy-file image #$output))))))
