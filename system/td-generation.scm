;; system/td-generation.scm — M10.1: the bootc-style generation bundle.
;;
;; A "generation" is a bootc-style bootable OCI image (M10-design.md): td's
;; existing OCI userspace image MADE BOOTABLE by carrying a matched kernel +
;; initrd. The stock OCI lowering (`image-with-os docker-image`) emits userspace
;; ONLY — no kernel, no initrd, no bootloader (checked, M10-design.md "What a
;; generation bundle is"). So this module takes that reproducible userspace image
;; and APPENDS a second layer carrying /boot/{bzImage,initrd.cpio.gz} from the
;; SAME operating-system, producing ONE ordinary, loadable, reproducible OCI image.
;;
;; The initrd is built from THIS generation's operating-system, whose root is the
;; distinct per-generation label (system/td-typed.scm slice 1) — so the bundle's
;; initrd mounts that generation's own root, not the shared td-root. The M10.2
;; placer extracts /boot and adds the GRUB entry that selects it.
;;
;; Reproducibility (prime directive 1): the userspace layer is Guix's own
;; reproducible packer; the appended /boot layer is a deterministic tar (sorted
;; names, fixed mtime/owner); the layer diff_id (sha256 of the uncompressed
;; layer.tar) and the image's manifest.json / config.json (Layers, rootfs.diff_ids,
;; history) are updated consistently, then the whole image is repacked with a
;; timestamp-free gzip. `guix build --check` is the oracle that this is bit-for-bit.
(define-module (system td-generation)
  #:use-module (gnu)
  #:use-module (gnu system)               ;operating-system-kernel-file, -initrd-file
  #:use-module (gnu system image)         ;image-with-os, docker-image, system-image
  #:use-module (gnu packages base)        ;tar
  #:use-module (gnu packages compression) ;gzip
  #:use-module (gnu packages guile)       ;guile-json-4
  #:use-module (gnu packages gnupg)       ;guile-gcrypt
  #:use-module (guix gexp)
  #:use-module (guix monads)
  #:use-module (guix store)
  #:use-module (ice-9 format)
  #:use-module (system td-typed)
  #:export (td-generation-image))

;; Build a bootc-style OCI image for the td CONFIG: the reproducible userspace
;; docker image plus a /boot layer carrying this generation's kernel + initrd.
;; Returns a monadic derivation (like `docker-image`), suitable for `run-with-store`.
(define* (td-generation-image config)
  ;; A generation image is meaningless without a generation: with generation #f
  ;; the config's root is the shared `td-root`, so the bundle's initrd would mount
  ;; the SAME filesystem as every other generation and rollback would be a no-op —
  ;; the very invariant this module exists to provide. Reject it at the API
  ;; boundary rather than emit a shared-root "generation" (P1). (td-config already
  ;; validates that a non-#f generation is a positive integer.)
  (let ((gen (td-config-generation config)))
    (unless gen
      (error (string-append
              "td-generation-image: requires a generation id — the config has "
              "generation #f, whose root is the shared td-root. A bootc generation "
              "image must mount its OWN per-generation root; build it from "
              "(td-config #:generation N)."))))
  (let* ((gen      (td-config-generation config))
         (label    (td-config-effective-root-label config))
         (os       (td-config->operating-system config))
         (base-img (system-image (image-with-os docker-image os)))
         (kernel   (operating-system-kernel-file os))
         (initrd   (operating-system-initrd-file os))
         (name     (format #f "td-generation-image-gen-~a" gen)))
    (gexp->derivation name
      (with-extensions (list guile-gcrypt guile-json-4)
        (with-imported-modules '((guix build utils))
          #~(begin
              (use-modules (guix build utils)
                           (gcrypt hash)
                           (gcrypt base16)
                           (json))

              (define tar  #$(file-append tar "/bin/tar"))
              (define gzip #$(file-append gzip "/bin/gzip"))

              ;; Deterministic tar: sorted names, fixed mtime (epoch+1, matching
              ;; the docker "created" stamp) and owner — so the layer and the
              ;; repacked image are bit-for-bit reproducible.
              (define tar-flags
                '("--sort=name" "--mtime=@1" "--owner=0" "--group=0"
                  "--numeric-owner"))
              (define (det-tar dir member outfile)
                (apply invoke tar (append tar-flags
                                          (list "-cf" outfile "-C" dir member))))

              ;; Replace or append KEY -> VAL in an alist, preserving order — used
              ;; to edit the parsed manifest/config without disturbing other keys.
              (define (alist-set alist key val)
                (let loop ((a alist) (seen #f) (acc '()))
                  (cond
                   ((null? a)
                    (reverse (if seen acc (cons (cons key val) acc))))
                   ((string=? (caar a) key)
                    (loop (cdr a) #t (cons (cons key val) acc)))
                   (else (loop (cdr a) seen (cons (car a) acc))))))
              (define (vsnoc vec x)        ;append X to a vector, as a vector
                (list->vector (append (vector->list vec) (list x))))

              ;; 1. Extract the base userspace image into img/.
              (mkdir-p "img")
              (invoke tar "--use-compress-program" gzip "-xf" #$base-img "-C" "img")

              ;; 2. Stage /boot with THIS generation's kernel + initrd, fixed modes.
              ;; Also write boot/td-identity — the generation id + root label this
              ;; image IS — so the M10.2 placer can BIND the image to the
              ;; --generation/--root-label it is placed as and reject a mismatch
              ;; (a gen-2 image installed under gen-1 would otherwise produce a menu
              ;; entry that lies about what it boots). Deterministic (fixed strings),
              ;; so it does not disturb reproducibility.
              (mkdir-p "stage/boot")
              (copy-file #$kernel "stage/boot/bzImage")
              (copy-file #$initrd "stage/boot/initrd.cpio.gz")
              (call-with-output-file "stage/boot/td-identity"
                (lambda (p)
                  (format p "generation=~a~%root-label=~a~%"
                          #$(number->string gen) #$label)))
              (chmod "stage/boot" #o755)
              (chmod "stage/boot/bzImage" #o444)
              (chmod "stage/boot/initrd.cpio.gz" #o444)
              (chmod "stage/boot/td-identity" #o444)

              ;; 3. Deterministic boot layer tar; diff_id = sha256(layer.tar).
              (det-tar "stage" "boot" "boot-layer.tar")
              (define diff-id
                (bytevector->base16-string
                 (file-hash (hash-algorithm sha256) "boot-layer.tar")))

              ;; 4. Place the layer dir (named by its diff_id) + legacy metadata,
              ;;    mirroring the shape of the base image's own layer dir.
              (define layer-dir (string-append "img/" diff-id))
              (mkdir-p layer-dir)
              (rename-file "boot-layer.tar" (string-append layer-dir "/layer.tar"))
              (call-with-output-file (string-append layer-dir "/VERSION")
                (lambda (p) (display "1.0" p)))
              (call-with-output-file (string-append layer-dir "/json")
                (lambda (p)
                  (display (string-append
                            "{\"id\":\"" diff-id "\","
                            "\"created\":\"1970-01-01T00:00:01Z\","
                            "\"container_config\":null}")
                           p)))

              ;; 5. manifest.json: append the boot layer to Layers.
              (let* ((manifest (call-with-input-file "img/manifest.json" json->scm))
                     (m0 (vector-ref manifest 0))
                     (layers (assoc-ref m0 "Layers"))
                     (m0* (alist-set m0 "Layers"
                                     (vsnoc layers
                                            (string-append diff-id "/layer.tar")))))
                (call-with-output-file "img/manifest.json"
                  (lambda (p) (scm->json (vector m0*) p))))

              ;; 6. config.json: append the layer's diff_id + a history entry.
              (let* ((cfg (call-with-input-file "img/config.json" json->scm))
                     (rootfs (assoc-ref cfg "rootfs"))
                     (diff-ids (assoc-ref rootfs "diff_ids"))
                     (rootfs* (alist-set rootfs "diff_ids"
                                         (vsnoc diff-ids
                                                (string-append "sha256:" diff-id))))
                     (history (or (assoc-ref cfg "history") #()))
                     (entry (list (cons "created" "1970-01-01T00:00:01Z")
                                  (cons "created_by" "td: bootc /boot layer")
                                  (cons "comment" "td generation bundle")))
                     (cfg* (alist-set (alist-set cfg "rootfs" rootfs*)
                                      "history" (vsnoc history entry))))
                (call-with-output-file "img/config.json"
                  (lambda (p) (scm->json cfg* p))))

              ;; 7. Repack deterministically; gzip -n (no name/timestamp) -> output.
              (apply invoke tar
                     (append tar-flags (list "-cf" "image.tar" "-C" "img" ".")))
              (invoke gzip "-9n" "image.tar")
              (copy-file "image.tar.gz" #$output)))))))
