;; tests/generation-image-check.scm — M10.1 bootc image artifact validation.
;;
;; The Makefile `generation-image` rung builds and `--check`s the bootc images,
;; then hands their store paths to THIS script via env vars. We crack them with
;; STRUCTURED tools — guile-json for the OCI metadata, guile-zlib for the initrd —
;; rather than whole-file greps, and assert:
;;
;;   (1) /boot present AND WIRED — each bootc image has a layer carrying
;;       /boot/{bzImage,initrd.cpio.gz}, and that layer is referenced in BOTH
;;       manifest.json's Layers vector AND config.json's rootfs.diff_ids vector.
;;       Field-specific: a stray occurrence of the hash elsewhere in the JSON does
;;       NOT pass (the earlier whole-file grep would have).
;;   (2) discriminator — the plain userspace image (TD_BASE_IMG) carries NO /boot.
;;   (3) correct per-generation ROOT — generation N's initrd embeds its OWN root
;;       label (computed from the typed compiler so it cannot drift) and NOT any
;;       other generation's. This proves each bundle SELECTS its own root, not
;;       merely that the two initrds happen to differ.
;;
;; Listings are read to EOF via open-pipe* (execs tar directly — no /bin/sh, and
;; no SIGPIPE/pipefail trap that a `tar | grep -q` pipeline would hit). Env:
;; TD_GEN1_IMG, TD_GEN2_IMG, TD_BASE_IMG. Run via `guix repl` so (json)/(zlib)/the
;; rnrs ports are on the load path. Exits non-zero on any failure.
(use-modules (guix build utils)
             (json)
             (zlib)
             (rnrs io ports)
             (rnrs bytevectors)
             (ice-9 popen)
             (ice-9 textual-ports)
             (ice-9 ftw)
             (ice-9 format)
             (srfi srfi-1)
             (srfi srfi-13)
             (system td-typed))

(define failures 0)
(define (fail fmt . args)
  (set! failures (+ failures 1))
  (apply format #t (string-append "FAIL: " fmt "~%") args))

(define (must k)
  (or (getenv k)
      (begin (format #t "FAIL: env ~a not set~%" k) (exit 1))))

(define scratch
  (let ((d (string-append (or (getenv "TMPDIR") "/tmp") "/td-genimg-check")))
    (when (file-exists? d) (system* "rm" "-rf" d))
    (mkdir-p d)
    d))

;; Full listing of a tar, drained to EOF. open-pipe* execs tar directly (no shell)
;; and we read the whole port, so there is no SIGPIPE that grep -q would induce.
(define (tar-list path)
  (let* ((p (open-pipe* OPEN_READ "tar" "-tf" path))
         (s (get-string-all p)))
    (close-pipe p)
    s))

(define (listing-has? listing name)
  (any (lambda (line)
         (or (string=? line name)
             (string=? line (string-append "./" name))
             (string-suffix? (string-append "/" name) line)))
       (string-split listing #\newline)))

(define (extract img dest)
  (mkdir-p dest)
  (invoke "tar" "xzf" img "-C" dest))

(define (layer-hexes dest)
  (filter (lambda (d)
            (file-exists? (string-append dest "/" d "/layer.tar")))
          (or (scandir dest (lambda (n) (not (member n '("." "..")))))
              '())))

;; manifest.json is a 1-element array of objects; return its Layers as a list.
(define (manifest-layers dest)
  (let ((m (call-with-input-file (string-append dest "/manifest.json") json->scm)))
    (vector->list (assoc-ref (vector-ref m 0) "Layers"))))

(define (config-diff-ids dest)
  (let* ((c (call-with-input-file (string-append dest "/config.json") json->scm))
         (rootfs (assoc-ref c "rootfs")))
    (vector->list (assoc-ref rootfs "diff_ids"))))

;; Decompress an initrd to a latin-1 string (byte<->char 1:1) for substring search.
(define (initrd-text gz)
  (let ((bv (call-with-gzip-input-port (open-input-file gz)
              (lambda (p) (get-bytevector-all p)))))
    (bytevector->string bv (make-transcoder (latin-1-codec)))))

;; Validate one bootc image. EXPECT-LABEL is this generation's root label;
;; FORBID-LABELS are other generations' labels that must NOT appear in its initrd.
(define (check-bootc-image label img expect-label forbid-labels)
  (let* ((dest (string-append scratch "/" label)))
    (extract img dest)
    (let* ((hexes (layer-hexes dest))
           (boot-hex
            (find (lambda (h)
                    (listing-has?
                     (tar-list (string-append dest "/" h "/layer.tar"))
                     "boot/bzImage"))
                  hexes)))
      (cond
       ((not boot-hex)
        (fail "~a: no layer carries /boot/bzImage — image is not bootable" label))
       (else
        ;; initrd present in the boot layer?
        (unless (listing-has?
                 (tar-list (string-append dest "/" boot-hex "/layer.tar"))
                 "boot/initrd.cpio.gz")
          (fail "~a: boot layer lacks /boot/initrd.cpio.gz" label))
        ;; (1) FIELD-SPECIFIC metadata linkage
        (unless (member (string-append boot-hex "/layer.tar")
                        (manifest-layers dest))
          (fail "~a: boot layer ~a is not in manifest.json Layers (orphaned layer)"
                label boot-hex))
        (unless (member (string-append "sha256:" boot-hex)
                        (config-diff-ids dest))
          (fail "~a: boot layer ~a diff_id is not in config.json rootfs.diff_ids (orphaned layer)"
                label boot-hex))
        ;; (3) correct per-generation root embedded in the initrd
        (let ((into (string-append dest "/boot-extract")))
          (mkdir-p into)
          (invoke "tar" "xf" (string-append dest "/" boot-hex "/layer.tar")
                  "-C" into)
          (let ((text (initrd-text (string-append into "/boot/initrd.cpio.gz"))))
            (unless (string-contains text expect-label)
              (fail "~a: initrd does NOT embed its own root label ~s — it would not mount this generation's root"
                    label expect-label))
            (for-each
             (lambda (bad)
               (when (string-contains text bad)
                 (fail "~a: initrd embeds a FOREIGN root label ~s — wrong/cross root selection"
                       label bad)))
             forbid-labels)))
        (when (zero? failures)
          (format #t "  ok: ~a carries /boot wired into manifest+config; initrd selects ~s~%"
                  label expect-label)))))))

;; The plain userspace image must carry NO /boot (the discriminator).
(define (check-no-boot label img)
  (let ((dest (string-append scratch "/" label)))
    (extract img dest)
    (for-each
     (lambda (h)
       (when (listing-has? (tar-list (string-append dest "/" h "/layer.tar"))
                           "boot/bzImage")
         (fail "~a: the plain userspace image already carries /boot — discriminator broken"
               label)))
     (layer-hexes dest))
    (format #t "  ok: ~a has no /boot (discriminator holds)~%" label)))

;; Expected per-generation labels — from the SAME compiler logic the images were
;; built with, so the assertion cannot drift from the implementation.
(define gen1-label (td-config-effective-root-label (td-config #:generation 1)))
(define gen2-label (td-config-effective-root-label (td-config #:generation 2)))

(format #t "~%== M10.1 bootc image artifact validation ==~%")
(format #t "  gen1 expected root: ~s   gen2 expected root: ~s~%" gen1-label gen2-label)
(check-bootc-image "gen1" (must "TD_GEN1_IMG") gen1-label (list gen2-label))
(check-bootc-image "gen2" (must "TD_GEN2_IMG") gen2-label (list gen1-label))
(check-no-boot "base" (must "TD_BASE_IMG"))

(if (zero? failures)
    (begin
      (format #t "PASS: both bootc images carry /boot WIRED into their OCI metadata \
(field-specific), the userspace image has none, and each generation's initrd \
selects its OWN per-generation root.~%")
      (exit 0))
    (begin
      (format #t "~a check(s) failed.~%" failures)
      (exit 1)))
