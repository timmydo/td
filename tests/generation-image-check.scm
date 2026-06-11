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
;;   (2) OCI metadata is well-formed — manifest.json's Config and every Layers
;;       entry name a file that actually exists in the image (a dangling Config or
;;       layer reference fails).
;;   (3) discriminator — the plain userspace image (TD_BASE_IMG) carries NO /boot
;;       AT ALL (any boot/ path in any layer, not just boot/bzImage).
;;   (4) correct per-generation ROOT — generation N's initrd embeds EXACTLY its own
;;       root label (computed from the typed compiler so it cannot drift) and NO
;;       other generation's label. We enumerate every `<base>-gen-<digits>` token
;;       in the initrd and require the set to equal {its own} — so gen-10 passed as
;;       gen-1 fails (boundary-aware, not a substring match) and an unexpected
;;       gen-7 would fail too (forbids ALL other generations, not a fixed list).
;;   (5) identity BINDING (M10.3) — boot/td-identity carries generation=N and
;;       root-label= (what the placer verifies), system= EQUAL to the typed
;;       compiler's lowered system path for this generation AND present in the
;;       image's own USERSPACE layer (so the GRUB entry the placer writes —
;;       gnu.system=/gnu.load= — names a path that exists in the root it boots:
;;       the bundle is bootable, not just bootable-looking), and root-uuid= EQUAL
;;       to the deterministic per-OS UUID (operating-system-uuid 'dce) the placer
;;       gives the mkfs'd root.
;;
;; Listings are read to EOF via open-pipe* (execs tar directly — no /bin/sh, and
;; no SIGPIPE/pipefail trap that a `tar | grep -q` pipeline would hit), and we
;; CHECK tar's exit status so a truncated/corrupt layer hard-fails instead of
;; silently yielding a short listing. Env: TD_GEN1_IMG, TD_GEN2_IMG, TD_BASE_IMG.
;; Run via `guix repl` so (json)/(zlib)/the rnrs ports are on the load path. Exits
;; non-zero on any failure.
(use-modules (guix build utils)
             (guix)                     ;with-store, run-with-store, mbegin
             (guix monads)              ;%store-monad, mbegin
             (gnu system)               ;operating-system-derivation, -uuid
             (gnu system uuid)          ;uuid->string
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

;; Unique per-invocation scratch dir — concurrent validators (the rung may run
;; alongside others) must not share a path and delete each other's extractions.
(define scratch
  (let ((d (format #f "~a/td-genimg-check-~a-~a"
                   (or (getenv "TMPDIR") "/tmp") (getpid) (random 1000000))))
    (mkdir-p d)
    d))

;; Full listing of a tar, drained to EOF. open-pipe* execs tar directly (no shell)
;; and we read the whole port, so there is no SIGPIPE that grep -q would induce.
;; We CHECK the exit status: a truncated/corrupt layer.tar makes `tar -tf` exit
;; non-zero, and we must NOT trust (or silently accept) its partial listing.
(define (tar-list path)
  (let* ((p (open-pipe* OPEN_READ "tar" "-tf" path))
         (s (get-string-all p))
         (status (close-pipe p)))
    (unless (and (integer? status) (zero? (status:exit-val status)))
      (error (format #f "tar -tf ~a failed (wait status ~s) — cannot trust listing"
                     path status)))
    s))

(define (normalize-line line)
  (if (string-prefix? "./" line) (substring line 2) line))

(define (listing-has? listing name)
  (any (lambda (line)
         (let ((l (normalize-line line)))
           (or (string=? l name)
               (string-suffix? (string-append "/" name) l))))
       (string-split listing #\newline)))

;; Any path under boot/ (boot, boot/, boot/anything) — the discriminator must
;; reject a base image that carries even a stray /boot/marker, not just bzImage.
(define (listing-has-boot? listing)
  (any (lambda (line)
         (let ((l (normalize-line line)))
           (or (string=? l "boot") (string=? l "boot/")
               (string-prefix? "boot/" l))))
       (string-split listing #\newline)))

(define (extract img dest)
  (mkdir-p dest)
  (invoke "tar" "xzf" img "-C" dest))

(define (layer-hexes dest)
  (filter (lambda (d)
            (file-exists? (string-append dest "/" d "/layer.tar")))
          (or (scandir dest (lambda (n) (not (member n '("." "..")))))
              '())))

;; manifest.json is a 1-element array of objects; return its first object.
(define (manifest0 dest)
  (vector-ref
   (call-with-input-file (string-append dest "/manifest.json") json->scm) 0))

(define (manifest-layers dest)
  (vector->list (assoc-ref (manifest0 dest) "Layers")))

(define (config-diff-ids dest)
  (let* ((c (call-with-input-file (string-append dest "/config.json") json->scm))
         (rootfs (assoc-ref c "rootfs")))
    (vector->list (assoc-ref rootfs "diff_ids"))))

;; (2) OCI metadata well-formedness: manifest Config and every Layers entry must
;; name a file that actually exists in the extracted image. A manifest pointing at
;; a nonexistent config.json or layer.tar is malformed and must fail.
(define (check-oci-wellformed label dest)
  (let* ((m0 (manifest0 dest))
         (config (assoc-ref m0 "Config"))
         (layers (vector->list (assoc-ref m0 "Layers"))))
    (unless (and (string? config)
                 (file-exists? (string-append dest "/" config)))
      (fail "~a: manifest Config ~s does not name a file present in the image"
            label config))
    (for-each
     (lambda (ln)
       (unless (file-exists? (string-append dest "/" ln))
         (fail "~a: manifest Layers entry ~s does not exist in the image" label ln)))
     layers)))

;; Decompress an initrd to a latin-1 string (byte<->char 1:1) for token search.
(define (initrd-text gz)
  (let ((bv (call-with-gzip-input-port (open-input-file gz)
              (lambda (p) (get-bytevector-all p)))))
    (bytevector->string bv (make-transcoder (latin-1-codec)))))

;; The per-generation labels share a fixed base from the typed compiler; the only
;; distinguishing part is the trailing integer. Enumerate EVERY distinct
;; "<base>-gen-<digits>" token in TEXT (consuming the full digit run, so
;; "<base>-gen-1" is NOT a substring hit inside "<base>-gen-10").
(define gen-prefix (string-append (td-config-root-fs-label (td-config)) "-gen-"))

(define (embedded-gen-labels text)
  (let ((plen (string-length gen-prefix))
        (tlen (string-length text)))
    (let loop ((i 0) (acc '()))
      (let ((hit (string-contains text gen-prefix i)))
        (if (not hit)
            (reverse acc)
            (let scan ((j (+ hit plen)))
              (if (and (< j tlen) (char-numeric? (string-ref text j)))
                  (scan (+ j 1))
                  (let ((tok (substring text hit j)))
                    (loop j
                          (if (and (> j (+ hit plen))   ;at least one digit
                                   (not (member tok acc)))
                              (cons tok acc) acc))))))))))

;; Parse boot/td-identity ("key=value" lines) into an alist.
(define (parse-identity file)
  (filter-map (lambda (line)
                (let ((i (string-index line #\=)))
                  (and i (cons (substring line 0 i) (substring line (+ i 1))))))
              (string-split (call-with-input-file file get-string-all)
                            #\newline)))

;; The typed compiler's lowered system path for generation N — what the
;; identity's system= must equal (and what the placer's menu will boot). Cheap:
;; derivation computation only, nothing is built. Mirrors the operating-system
;; gexp compiler exactly (set-guile-for-build first — without it the lowering
;; yields a DIFFERENT system derivation than the one the image embeds).
(define (expected-system-path n)
  (with-store store
    (set-build-options store #:use-substitutes? #f #:offload? #f)
    (derivation->output-path
     (run-with-store store
       (mbegin %store-monad
         (set-guile-for-build (default-guile))
         (operating-system-derivation
          (td-config->operating-system (td-config #:generation n))))))))

;; Validate one bootc image for generation N. EXPECT-LABEL is this generation's
;; root label; the initrd must embed EXACTLY it and no other generation's label.
(define (check-bootc-image label img n expect-label)
  (let ((dest (string-append scratch "/" label)))
    (extract img dest)
    (check-oci-wellformed label dest)
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
        ;; (4) EXACT per-generation root embedded in the initrd
        (let ((into (string-append dest "/boot-extract")))
          (mkdir-p into)
          (invoke "tar" "xf" (string-append dest "/" boot-hex "/layer.tar")
                  "-C" into)
          (let* ((text  (initrd-text (string-append into "/boot/initrd.cpio.gz")))
                 (found (embedded-gen-labels text)))
            (cond
             ((not (member expect-label found))
              (fail "~a: initrd does NOT embed its own root label ~s (gen labels found: ~s) — it would not mount this generation's root"
                    label expect-label found))
             ((not (equal? found (list expect-label)))
              (fail "~a: initrd embeds FOREIGN root label(s) ~s — expected only ~s (wrong/cross root selection)"
                    label
                    (filter (lambda (x) (not (string=? x expect-label))) found)
                    expect-label))))
          ;; (5) identity BINDING — td-identity must say what this image IS
          ;; (generation/root-label, verified by the placer) and what makes it
          ;; BOOT (system=/root-uuid=, consumed by the placer's menu + mkfs).
          (let ((idf (string-append into "/boot/td-identity")))
            (if (not (file-exists? idf))
                (fail "~a: boot layer carries no boot/td-identity" label)
                (let* ((id        (parse-identity idf))
                       (id-gen    (assoc-ref id "generation"))
                       (id-label  (assoc-ref id "root-label"))
                       (id-system (assoc-ref id "system"))
                       (id-uuid   (assoc-ref id "root-uuid"))
                       (exp-sys   (expected-system-path n))
                       (exp-uuid  (uuid->string
                                   (operating-system-uuid
                                    (td-config->operating-system
                                     (td-config #:generation n))
                                    'dce))))
                  (unless (equal? id-gen (number->string n))
                    (fail "~a: identity generation=~s != expected ~a" label id-gen n))
                  (unless (equal? id-label expect-label)
                    (fail "~a: identity root-label=~s != expected ~s"
                          label id-label expect-label))
                  (unless (equal? id-uuid exp-uuid)
                    (fail "~a: identity root-uuid=~s != deterministic per-OS uuid ~s"
                          label id-uuid exp-uuid))
                  (cond
                   ((not (equal? id-system exp-sys))
                    (fail "~a: identity system=~s != the typed compiler's system path ~s"
                          label id-system exp-sys))
                   (else
                    ;; The system the menu will gnu.system=/gnu.load= must EXIST
                    ;; in the root the menu mounts — i.e. in a USERSPACE layer
                    ;; (not the boot layer): bootable, not bootable-looking.
                    (let* ((userspace (remove
                                       (lambda (lt)
                                         (string=? lt (string-append
                                                       boot-hex "/layer.tar")))
                                       (manifest-layers dest)))
                           (sys-boot (string-append (substring exp-sys 1)
                                                    "/boot")))
                      (unless (any (lambda (lt)
                                     (listing-has?
                                      (tar-list (string-append dest "/" lt))
                                      sys-boot))
                                   userspace)
                        (fail "~a: identity system ~s (+ /boot) is NOT in any userspace layer — the GRUB entry would point at a path missing from the root it boots"
                              label exp-sys)))))))))
        (when (zero? failures)
          (format #t "  ok: ~a carries /boot wired into manifest+config (well-formed); initrd selects exactly ~s; identity binds gen/label/system/uuid~%"
                  label expect-label)))))))

;; The plain userspace image must carry NO /boot AT ALL (the discriminator) — any
;; boot/ path in any layer, not merely boot/bzImage.
(define (check-no-boot label img)
  (let ((dest (string-append scratch "/" label)))
    (extract img dest)
    (for-each
     (lambda (h)
       (when (listing-has-boot? (tar-list (string-append dest "/" h "/layer.tar")))
         (fail "~a: the plain userspace image already carries a /boot path — discriminator broken"
               label)))
     (layer-hexes dest))
    (when (zero? failures)
      (format #t "  ok: ~a has no /boot path in any layer (discriminator holds)~%"
              label))))

;; Expected per-generation labels — from the SAME compiler logic the images were
;; built with, so the assertion cannot drift from the implementation.
(define gen1-label (td-config-effective-root-label (td-config #:generation 1)))
(define gen2-label (td-config-effective-root-label (td-config #:generation 2)))

(format #t "~%== M10.1 bootc image artifact validation ==~%")
(format #t "  gen1 expected root: ~s   gen2 expected root: ~s~%" gen1-label gen2-label)
(check-bootc-image "gen1" (must "TD_GEN1_IMG") 1 gen1-label)
(check-bootc-image "gen2" (must "TD_GEN2_IMG") 2 gen2-label)
(check-no-boot "base" (must "TD_BASE_IMG"))

(if (zero? failures)
    (begin
      (format #t "PASS: both bootc images carry /boot WIRED into well-formed OCI \
metadata (field-specific), the userspace image has none, each generation's \
initrd selects EXACTLY its own per-generation root, and each identity binds \
generation/root-label/system/root-uuid with system= present in the userspace \
layer the menu will boot.~%")
      (exit 0))
    (begin
      (format #t "~a check(s) failed.~%" failures)
      (exit 1)))
