;; system/td-place.scm — M10.2: wire the guix-free PLACER into a reproducible,
;; behavioral test derivation.
;;
;; The placer itself (system/td-place.sh) is the deliverable: a POSIX shell tool
;; that runs ON THE TARGET (which has no guix) to extract /boot from a bootc
;; generation image, write a per-generation GRUB menu entry that selects that
;; generation's own root, and prune old generations (M10-design.md step 3,
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
;;
;; The deployment side is tested against BEHAVIOR, not diffed against a Guix
;; component it does not have (M10-design.md decision 2): tests/place-check.scm
;; cracks the produced tree and asserts placement, the per-generation menu, root
;; selection, preamble preservation, and pruning.
(define-module (system td-place)
  #:use-module (gnu packages base)        ;tar, coreutils, sed, grep
  #:use-module (gnu packages bash)        ;bash
  #:use-module (gnu packages compression) ;gzip
  #:use-module (guix gexp)
  #:use-module (guix monads)
  #:use-module (guix store)
  #:use-module (system td-typed)
  #:use-module (system td-generation)
  #:export (td-placed-tree))

;; Build a target /boot tree by PLACING each generation in GENS (in order, into
;; the same target) with the guix-free placer, pruning to the newest KEEP. The
;; output is the resulting target tree: boot/td/gen-N/{bzImage,initrd.cpio.gz,
;; root-label} per kept generation, plus boot/grub/grub.cfg (a user preamble the
;; placer must preserve + the placer's marker-delimited managed block). Returns a
;; monadic derivation (suitable for `run-with-store`).
(define* (td-placed-tree #:key (gens '(1 2)) (keep 10))
  (mlet %store-monad
      ((images (mapm %store-monad
                     (lambda (n) (td-generation-image (td-config #:generation n)))
                     gens)))
    (let* ((labels (map (lambda (n)
                          (td-config-effective-root-label (td-config #:generation n)))
                        gens))
           ;; One (gen-string image-path root-label) job per generation. `#$img`
           ;; lowers the bootc image derivation to its output path.
           (jobs   (map (lambda (n img label)
                          #~(list #$(number->string n) #$img #$label))
                        gens images labels)))
      (gexp->derivation "td-placed-tree"
        (with-imported-modules '((guix build utils))
          #~(begin
              (use-modules (guix build utils) (ice-9 match))

              (define placer #$(local-file "td-place.sh"))
              (define sh #$(file-append bash "/bin/bash"))

              ;; Guix-free by construction: ONLY base tools on PATH, no guix.
              (setenv "PATH"
                      (string-append #$(file-append coreutils "/bin") ":"
                                     #$(file-append tar "/bin") ":"
                                     #$(file-append gzip "/bin") ":"
                                     #$(file-append sed "/bin") ":"
                                     #$(file-append grep "/bin")))

              (define target   (string-append (getcwd) "/target"))
              (define boot      (string-append target "/boot"))
              (define grub-cfg  (string-append boot "/grub/grub.cfg"))
              (mkdir-p (string-append boot "/grub"))

              ;; Seed grub.cfg with a user preamble the placer MUST preserve — it
              ;; may only ever touch its own marker-delimited managed block.
              (call-with-output-file grub-cfg
                (lambda (p)
                  (display "\
# td target grub.cfg — user preamble (must be preserved by td-place)
set timeout=5
" p)))

              ;; Place each generation in order into the same target.
              (for-each
               (match-lambda
                 ((gen img label)
                  (invoke sh placer
                          "--image"      img
                          "--generation" gen
                          "--root-label" label
                          "--boot-dir"   boot
                          "--grub-cfg"   grub-cfg
                          "--keep"       #$(number->string keep))))
               (list #$@jobs))

              (copy-recursively target #$output)))))))
