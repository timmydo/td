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
;;
;;   * `td-verity-mapped-device` — the <mapped-device> a generation system
;;     boots through (S2): the initrd's pre-mount opens the generation's root
;;     partition (found by its per-generation LABEL — still the slot-binding
;;     mechanism) as the dm-verity target `/dev/mapper/td-root`, taking the
;;     root hash and hash offset from the kernel command line
;;     (td.roothash=/td.hashoffset=, written by the placer's menuentry).
;;     FAIL CLOSED by construction: there is no fallback to a plain mount —
;;     a missing or wrong parameter, or a hash tree that does not match the
;;     data, leaves the system without its store device and the boot stops.
(define-module (system td-verity)
  #:use-module (gnu packages cryptsetup)
  #:use-module (gnu system file-systems) ;file-system-label
  #:use-module (gnu system mapped-devices)
  #:use-module (guix gexp)
  #:use-module (guix packages)
  #:use-module (guix utils)              ;substitute-keyword-arguments
  #:use-module (ice-9 match)
  #:export (veritysetup-static
            %td-verity-target
            td-verity-device-mapping
            td-verity-mapped-device))

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

;;;
;;; The verity mapped device (M11 S2) — how a generation system reaches its
;;; sealed store at boot.
;;;

;; The device-mapper target name; the generation's store file-system mounts
;; /dev/mapper/<this> read-only at /gnu/store.
(define %td-verity-target "td-root")

(define (open-td-verity-device source targets)
  "Return a gexp that opens SOURCE (a <file-system-label> — the
per-generation root partition's label) as the dm-verity device TARGET. The
root hash and the hash-area offset come from the kernel command line
(td.roothash= / td.hashoffset=), placed there by the placer's menuentry —
the image cannot carry its own root hash (DESIGN §2.7). No fallback path:
any missing parameter or verification mismatch fails the boot closed."
  (match targets
    ((target)
     (let ((label (if (file-system-label? source)
                      (file-system-label->string source)
                      source)))
       #~(let* ((args     (linux-command-line))
                (roothash (find-long-option "td.roothash" args))
                (offset   (find-long-option "td.hashoffset" args)))
           (unless (and roothash offset)
             (error "td-verity: td.roothash=/td.hashoffset= missing from the \
kernel command line — refusing to assemble an unverified root"))
           ;; The partition may take a moment to appear (same retry as
           ;; Guix's own LUKS open).
           (let ((partition
                  (or (let loop ((tries 0))
                        (or (find-partition-by-label #$label)
                            (and (< tries 20)
                                 (begin (sleep 1) (loop (+ tries 1))))))
                      (error "td-verity: no partition with label" #$label))))
             (zero? (system* #$(file-append veritysetup-static
                                            "/sbin/veritysetup")
                             "open" partition #$target partition roothash
                             "--hash-offset" offset))))))))

(define td-verity-device-mapping
  ;; The type of td's dm-verity mapped devices. No close procedure: a verity
  ;; target is read-only kernel state with nothing to flush, and a generation
  ;; system holds its store on it until power-off.
  (mapped-device-kind
   (open open-td-verity-device)
   (modules '(((gnu build linux-boot)
               #:select (linux-command-line find-long-option))
              ((gnu build file-systems)
               #:select (find-partition-by-label))))))

(define (td-verity-mapped-device label)
  "The <mapped-device> opening the partition labeled LABEL (this
generation's root, slot-bound by the placer) as /dev/mapper/td-root."
  (mapped-device
   (source (file-system-label label))
   (targets (list %td-verity-target))
   (type td-verity-device-mapping)))
