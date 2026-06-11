;; system/td-generation.scm — M10.1/M10.3: the bootc-style generation bundle.
;;
;; A "generation" is a bootc-style bootable OCI image (M10-design.md): a
;; reproducible OCI image carrying td's userspace MADE BOOTABLE by a /boot layer
;; with a matched kernel + initrd. The M10.2 placer extracts /boot, applies the
;; userspace layers as that generation's own root, and adds the GRUB entry that
;; selects it.
;;
;; Userspace layer (revised in M10.3). td's stock OCI lowering
;; (`image-with-os docker-image`) wraps the OS in `containerized-operating-system`:
;; dummy kernel, getty/networking stripped, file-systems removed. Fine for a
;; container runtime — but a generation's root must carry the system you actually
;; BOOT (M10.3's rollback test asserts /run/current-system is THIS generation's
;; system). So the bundle's userspace layer packs the REAL operating-system's
;; closure — `initialize-root-partition` + `build-docker-image`, exactly the
;; content Guix puts in a disk image's root partition, as an OCI layer. The plain
;; containerized image remains what `make oci` ships (and the generation-image
;; rung's no-/boot discriminator); the BUNDLE is the bootable one.
;;
;; The initrd is built from THIS generation's operating-system, whose root is the
;; distinct per-generation label (system/td-typed.scm slice 1) — so the bundle's
;; initrd mounts that generation's own root, not the shared td-root.
;;
;; boot/td-identity binds the image to what it claims to be (the placer verifies
;; before placing, M10.2) and carries what the placer needs to make it BOOT
;; (M10.3):
;;   generation=N      root-label=td-root-gen-N   — the binding (M10.2)
;;   system=/gnu/store/...-system                 — what the GRUB entry's
;;       gnu.system=/gnu.load= must point at; it is IN the userspace layer
;;   root-uuid=<operating-system-uuid os 'dce>    — deterministic per-OS UUID the
;;       placer gives the mkfs'd root (reproducible filesystem identity)
;;
;; Reproducibility (prime directive 1): the userspace layer is Guix's own
;; reproducible packer over the closure; the appended /boot layer is a
;; deterministic tar (sorted names, fixed mtime/owner); the layer diff_id and the
;; image's manifest.json / config.json are updated consistently; the whole image
;; is repacked with a timestamp-free gzip. `guix build --check` is the oracle.
(define-module (system td-generation)
  #:use-module (gnu)
  #:use-module (gnu system)               ;operating-system-kernel-file, -initrd-file
  #:use-module (gnu system uuid)          ;uuid->string
  #:use-module (gnu packages base)        ;tar, libc-utf8-locales-for-target
  #:use-module (gnu packages compression) ;gzip
  #:use-module (gnu packages guile)       ;guile-json-3/4, guile-sqlite3
  #:use-module (gnu packages gnupg)       ;guile-gcrypt
  #:use-module (guix gexp)
  #:use-module (guix modules)             ;source-module-closure
  #:use-module (guix monads)
  #:use-module (guix packages)            ;package-transitive-propagated-inputs
  #:use-module (guix store)
  #:use-module ((guix self) #:select (make-config.scm))
  #:use-module ((guix utils) #:select (%current-target-system
                                       nix-system->gnu-triplet))
  #:use-module (ice-9 format)
  #:use-module (ice-9 match)
  #:use-module (srfi srfi-1)
  #:use-module (system td-typed)
  #:export (td-generation-image))

;; Module-name filter for source-module-closure, as in (gnu system image):
;; select ONLY (guix ...) and (gnu ...) modules — everything else ((json),
;; (srfi ...), ...) comes from the extensions/Guile — and not (guix config)
;; (generated fresh below) nor (guix git) (autoloaded, would drag Guile-Git in).
(define neither-config-nor-git?
  (match-lambda
    (('guix 'config) #f)
    (('guix 'git) #f)
    (('guix rest ...) #t)
    (('gnu rest ...) #t)
    (rest #f)))

;; Guile-Gcrypt, Guile-SQLite3 and their propagated inputs — the extensions
;; (guix store database) needs to LOAD (we never register closures, but the
;; module graph of (gnu build image) imports it). Same set as (gnu system image).
(define gcrypt-sqlite3&co
  (append-map (lambda (package)
                (cons package
                      (match (package-transitive-propagated-inputs package)
                        (((labels packages) ...)
                         packages))))
              (list guile-gcrypt guile-sqlite3)))

;; Pack the REAL (non-containerized) OS closure as a single-layer docker image —
;; the bootable userspace of a generation bundle. This mirrors Guix's
;; `system-docker-image` builder, minus `containerized-operating-system` (the
;; whole point: the layer must hold the system the generation BOOTS, dummy-free)
;; and minus the container entry-point (a generation's entry point is its
;; kernel). Returns a file-like object.
(define (td-userspace-image name os)
  (define image-target
    (or (%current-target-system) (nix-system->gnu-triplet)))

  (define builder
    (with-extensions (cons guile-json-3 gcrypt-sqlite3&co)
      (with-imported-modules `(,@(source-module-closure
                                  '((guix docker)
                                    (guix store database)
                                    (guix build utils)
                                    (guix build store-copy)
                                    (gnu build image))
                                  #:select? neither-config-nor-git?)
                               ((guix config) => ,(make-config.scm)))
        #~(begin
            (use-modules (guix docker)
                         (guix build utils)
                         (guix build store-copy)
                         (gnu build image)
                         (srfi srfi-19))

            ;; Allow non-ASCII file names--e.g., 'nss-certs'--to be decoded.
            (setenv "GUIX_LOCPATH"
                    #+(file-append (libc-utf8-locales-for-target)
                                   "/lib/locale"))
            (setlocale LC_ALL "en_US.utf8")

            (set-path-environment-variable "PATH" '("bin" "sbin") '(#+tar))

            (let ((image-root (string-append (getcwd) "/tmp-root")))
              (mkdir-p image-root)
              ;; The essential non-store files (/etc, /var/guix profile links,
              ;; /bin/sh, ...) — the same population a disk image's root gets.
              (initialize-root-partition image-root
                                         #:references-graphs '("system-graph")
                                         #:copy-closures? #f
                                         #:register-closures? #f
                                         #:deduplicate? #f
                                         #:system-directory #$os)
              (build-docker-image #$output
                                  (append (list image-root)
                                          (map store-info-item
                                               (call-with-input-file
                                                   "system-graph"
                                                 read-reference-graph)))
                                  #$os
                                  #:compressor
                                  '(#+(file-append gzip "/bin/gzip") "-9n")
                                  #:creation-time (make-time time-utc 0 1)
                                  #:system #$image-target
                                  #:transformations
                                  `((,image-root -> ""))))))))

  (computed-file name builder
                 #:options `(#:references-graphs (("system-graph" ,os)))))

;; Build a bootc-style OCI image for the td CONFIG: the bootable userspace layer
;; (the real system closure) plus a /boot layer carrying this generation's kernel
;; + initrd + identity. TRANSFORM-OS (an operating-system -> operating-system
;; procedure, default identity) is applied before anything is derived from the
;; OS — the M10.3 rollback test uses it to add the marionette backdoor, keeping
;; bundle, identity and initrd self-consistent; the default leaves the shipped
;; bundles untouched. Returns a monadic derivation (suitable for
;; `run-with-store`).
(define* (td-generation-image config #:key (transform-os identity))
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
         (os       (transform-os (td-config->operating-system config)))
         (base-img (td-userspace-image
                    (format #f "td-userspace-image-gen-~a.tar.gz" gen) os))
         (kernel   (operating-system-kernel-file os))
         (initrd   (operating-system-initrd-file os))
         (root-uuid (uuid->string (operating-system-uuid os 'dce)))
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
              ;; image IS (so the M10.2 placer can BIND the image to the
              ;; --generation/--root-label it is placed as and reject a mismatch),
              ;; plus what the placer needs to make the placed generation BOOT
              ;; (M10.3): the system path its GRUB entry must gnu.system=/gnu.load=
              ;; (a path inside the userspace layer), and the deterministic UUID
              ;; for the mkfs'd per-generation root. Deterministic (fixed strings
              ;; + store path + content-derived UUID), so reproducibility holds.
              (mkdir-p "stage/boot")
              (copy-file #$kernel "stage/boot/bzImage")
              (copy-file #$initrd "stage/boot/initrd.cpio.gz")
              (call-with-output-file "stage/boot/td-identity"
                (lambda (p)
                  (format p "generation=~a~%root-label=~a~%system=~a~%root-uuid=~a~%"
                          #$(number->string gen) #$label #$os #$root-uuid)))
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
