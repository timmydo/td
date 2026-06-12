;; system/td-registry.scm — M12 S3: the signed, static distribution REGISTRY.
;;
;; DESIGN §2.7: a generation's identity is the digest of the distributed
;; artifact in its canonical form — since the oci-load track landed the
;; canonical OCI layout, that is the OCI image MANIFEST digest. The "registry"
;; is deliberately a static layout, not a product: pushing is populating a
;; content-addressed directory; pulling is reading blobs back out by digest;
;; HTTP would only ever serve these same bytes, so file transport inside the
;; offline loop exercises the real thing.
;;
;; `td-registry` builds that registry as a reproducible derivation:
;;
;;   oci/                      one canonical OCI layout (skopeo-produced;
;;                             oci-layout, index.json, blobs/sha256/<hex>),
;;                             one ref per pushed generation (`gen-N`), blobs
;;                             shared across generations by content addressing
;;   signatures/<hex>.digest   the one-line identity STATEMENT for manifest
;;                             digest sha256:<hex> — exactly "sha256:<hex>"
;;   signatures/<hex>.digest.sig
;;                             its detached signify (ed25519) signature by the
;;                             committed TEST key (tests/keys/README) — the
;;                             §2.7 pre-decision: sign the digest, never the
;;                             install ordinal; no sigstore
;;
;; skopeo (adopted by oci-load: foreign OCI implementation) converts each
;; Guix-built docker-archive generation image into the layout; signify
;; (adopted by the M12 S2 probe: 3 drvs offline from the pin) signs the
;; statements. Both are declared inputs — the build runs offline. Ed25519
;; signatures are deterministic (RFC 8032), so the whole registry passes
;; `guix build --check`.
;;
;; tests/registry-check.sh asserts the artifact's contract (statement ==
;; foreign-inspected manifest digest, signature verifies, every blob re-hashes
;; to its name, pull-by-digest walk) plus negative controls; the Makefile
;; `registry` rung wires it into the loop.
(define-module (system td-registry)
  #:use-module (gnu packages base)           ;coreutils
  #:use-module (gnu packages crypto)         ;signify
  #:use-module (gnu packages virtualization) ;skopeo
  #:use-module (guix gexp)
  #:use-module (guix monads)
  #:use-module (guix store)
  #:use-module (system td-typed)
  #:use-module (system td-generation)
  #:export (td-registry))

;; Build the registry holding the generation images for GENS (pushed in
;; order). TRANSFORM-OS passes through to td-generation-image (default
;; identity — the same images the place rung consumes). Returns a monadic
;; derivation (suitable for `run-with-store`).
(define* (td-registry #:key (gens '(1 2)) (transform-os identity))
  (mlet %store-monad
      ((images (mapm %store-monad
                     (lambda (n)
                       (td-generation-image (td-config #:generation n)
                                            #:transform-os transform-os))
                     gens)))
    (let ((jobs (map (lambda (n img)
                       #~(list #$(number->string n) #$img))
                     gens images)))
      (gexp->derivation "td-registry"
        (with-imported-modules '((guix build utils))
          #~(begin
              (use-modules (guix build utils) (ice-9 match)
                           (ice-9 popen) (ice-9 rdelim))

              (setenv "PATH"
                      (string-append #$(file-append skopeo "/bin") ":"
                                     #$(file-append signify "/bin") ":"
                                     #$(file-append coreutils "/bin")))

              ;; skopeo scratch space — the build sandbox has no /var/tmp, and
              ;; skopeo does not honor TMPDIR for its big files (the oci-load
              ;; lesson; its global --tmpdir flag does).
              (define tmp (string-append (getcwd) "/tmp"))
              (mkdir-p tmp)
              (mkdir-p (string-append #$output "/signatures"))

              ;; The committed TEST signing key (tests/keys/README), under a
              ;; fixed local name so the signature's untrusted comment names
              ;; the key, not a store path.
              (copy-file #$(local-file "../tests/keys/td_m12_signify.sec")
                         "td_m12_signify.sec")

              ;; The manifest digest of REF, re-derived by skopeo from the
              ;; layout just written — the §2.7 identity the statement signs.
              (define (inspect-digest ref)
                (let* ((port   (open-pipe* OPEN_READ
                                           "skopeo" "--tmpdir" tmp
                                           "inspect" "--format" "{{.Digest}}"
                                           ref))
                       (digest (read-line port))
                       (status (close-pipe port)))
                  (unless (and (zero? (status:exit-val status))
                               (string? digest)
                               (string-prefix? "sha256:" digest))
                    (error "skopeo inspect yielded no manifest digest"
                           ref digest))
                  digest))

              (for-each
               (match-lambda
                 ((gen img)
                  (let ((ref (string-append "oci:" #$output "/oci:gen-" gen)))
                    ;; PUSH: docker-archive -> the shared canonical layout.
                    (invoke "skopeo" "--tmpdir" tmp
                            "copy" "--insecure-policy"
                            (string-append "docker-archive:" img) ref)
                    ;; STATEMENT: this image's §2.7 identity, one line —
                    ;; signed detached with the td TEST key (deterministic
                    ;; ed25519, so `--check` holds).
                    (let* ((digest (inspect-digest ref))
                           (hex    (substring digest 7))
                           (stmt   (string-append #$output "/signatures/"
                                                  hex ".digest")))
                      (call-with-output-file stmt
                        (lambda (p) (format p "~a~%" digest)))
                      (invoke "signify" "-S"
                              "-s" "td_m12_signify.sec"
                              "-m" stmt
                              "-x" (string-append stmt ".sig"))))))
               (list #$@jobs))))))))
