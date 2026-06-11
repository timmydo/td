;; system/td-verity.scm — M11: verified generations (dm-verity over the
;; per-generation root image, DESIGN §7.1; mechanism settled 2026-06-10).
;;
;; A generation's root.img carries an APPENDED dm-verity hash tree (ChromeOS
;; style: data area + hash area share the artifact; the hash area starts at
;; --hash-offset = the data size). The placer creates it at --mkfs time with a
;; FIXED salt and the identity's deterministic UUID, so the whole image stays
;; bit-reproducible (`guix build --check` on the placed tree is the oracle).
;; The resulting root hash cannot live inside the image (self-reference,
;; DESIGN §2.7) so the placer RECORDS it next to the generation's boot files;
;; from S2 on, the menuentry it writes will carry the hash to the kernel on
;; the command line.
;;
;; This module provides the pieces Guix-side code needs:
;;
;;   * `veritysetup-static` — a statically-linked veritysetup for the initrd
;;     (and the place derivation). The S0 probe found Guix's own
;;     `cryptsetup-static` is built with --disable-veritysetup, so it cannot
;;     open verity devices; this variant re-enables veritysetup and keeps ONLY
;;     that binary, mirroring cryptsetup-static's own remove-cruft phase. All
;;     static-library inputs are inherited (shared derivations).
(define-module (system td-verity)
  #:use-module (gnu packages cryptsetup)
  #:use-module (guix gexp)
  #:use-module (guix packages)
  #:use-module (guix utils)              ;substitute-keyword-arguments
  #:export (veritysetup-static))

(define veritysetup-static
  ;; Statically-linked 'veritysetup' for use in initrds — exactly
  ;; cryptsetup-static minus the --disable-veritysetup flag, keeping
  ;; sbin/veritysetup instead of sbin/cryptsetup.
  (package
    (inherit cryptsetup-static)
    (name "veritysetup-static")
    (arguments
     (substitute-keyword-arguments (package-arguments cryptsetup-static)
       ((#:configure-flags flags)
        ;; veritysetup is enabled by default; dropping the --disable flag is
        ;; all it takes (--enable-static-cryptsetup already builds .static
        ;; binaries for every enabled tool).
        `(delete "--disable-veritysetup" ,flags))
       ((#:phases phases)
        #~(modify-phases #$phases
            (replace 'remove-cruft
              (lambda* (#:key outputs #:allow-other-keys)
                ;; Remove everything except the 'veritysetup' command.
                (let ((out (assoc-ref outputs "out")))
                  (with-directory-excursion out
                    (let ((dirs (scandir "."
                                         (match-lambda
                                           ((or "." "..") #f)
                                           (_ #t)))))
                      (for-each delete-file-recursively
                                (delete "sbin" dirs))
                      (for-each (lambda (file)
                                  (unless (string=? file "veritysetup.static")
                                    (delete-file
                                     (string-append "sbin/" file))))
                                (scandir "sbin"
                                         (match-lambda
                                           ((or "." "..") #f)
                                           (_ #t))))
                      (rename-file "sbin/veritysetup.static"
                                   "sbin/veritysetup")
                      (remove-store-references "sbin/veritysetup"))))))))))
    (synopsis "Statically-linked veritysetup command")
    (description "This package provides the @command{veritysetup} command,
statically linked, for use in initrds to open dm-verity integrity-verified
block devices.")))
