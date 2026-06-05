;; tests/manifest-diff.scm — M6 differential test (DESIGN.md §6: manifest-driven,
;; image-swap-only; §2.4 step 5+, §2.5).
;;
;; M6 makes the package contents of the image a declarative function of a
;; *manifest* (the typed config's `manifest` field), and forbids any imperative
;; `guix install`-style mutation: the only way to change what the image contains
;; is to declare a different manifest and rebuild the WHOLE image — a wholesale
;; swap, never an in-place install. The artifact under test is therefore the
;; OCI image derivation (the thing you would swap), exactly as in M5.
;;
;; Same self-discriminating shape as tests/oci-diff.scm (the M3 false-green
;; lesson, kept as a permanent guardrail). It asserts THREE things, each able to
;; go red:
;;
;;   (a) CONVERGE — the typed front-end's DEFAULT manifest (%base-packages, the
;;       operating-system field's own default) lowers to the SAME OCI image
;;       derivation as the frozen hand-written td-system oracle. The manifest
;;       layer is purely additive — the shipped default image is unchanged.
;;   (b) SWAP DISCRIMINATES — a config whose manifest adds ONE package (GNU
;;       hello) lowers to a DIFFERENT OCI image derivation. A new manifest is a
;;       new image generation, with its own identity. If the comparison ever
;;       stops distinguishing manifests, (b) fails — the red-run baked in.
;;   (c) MANIFEST DRIVES CONTENTS — the added package is actually present in the
;;       swapped system's package set and ABSENT from the default's. This proves
;;       (b)'s divergence is the manifest doing real work, not an incidental
;;       hash change, and that the default image is not secretly carrying it.
;;
;; (a)+(b) are the load-bearing self-discriminating pair (break the compiler's
;; default → (a) red; make the swap a no-op → (b) red). (c) pins the divergence
;; to the declared manifest. The bit-for-bit reproducibility of a SWAPPED
;; generation is proven separately by `make manifest-check` (a real
;; `guix build --check` on the hello-bearing OCI image).
;;
;; Run as a script so the process exit status is the test result (`guix repl
;; FILE` honors `(exit)`, unlike a script piped via STDIN — see typed-diff.scm).
(use-modules (guix store)
             (guix derivations)
             (guix gexp)
             (guix monads)
             (gnu)
             (gnu system)
             (gnu system image)
             (gnu packages base)        ;hello
             (srfi srfi-1)
             (ice-9 format)
             (system td)
             (system td-typed))

;; The swap package and the swapped manifest. hello is tiny, certainly-free
;; (GNU), and present in the pinned channel — so `make manifest-check` can also
;; build and --check the swapped image within the loop-latency budget (§1.3).
(define %swap-package hello)
(define %swapped-manifest (cons %swap-package %base-packages))

(with-store store
  ;; Honest offline (triage #1): forbid substitution for this store session. The
  ;; shared host daemon has network + nonguix in its substitute URLs (check.sh),
  ;; and `guix repl` does not read GUIX_BUILD_OPTIONS — so set it explicitly here.
  ;; This is also what stops the graft-driven substitute *queries* this rung's
  ;; first run made when `hello` was not yet warm.
  (set-build-options store #:use-substitutes? #f)

  ;; Lower an operating-system to the derivation of its Docker/OCI image and
  ;; return the .drv store path — identical to tests/oci-diff.scm. No image is
  ;; built here; this is a pure structural fingerprint of the OCI artifact.
  (define (oci-drv os)
    (derivation-file-name
     (run-with-store store
       (lower-object
        (system-image (image-with-os docker-image os))))))

  (define default-os  (td-config->operating-system (td-config)))
  (define swapped-os   (td-config->operating-system
                        (td-config #:manifest %swapped-manifest)))

  (let* ((oracle  (oci-drv td-system))
         (default (oci-drv default-os))
         (swapped (oci-drv swapped-os))
         (converge?     (string=? oracle default))
         (discriminate? (not (string=? oracle swapped)))
         ;; (c): the manifest, and only the manifest, decides contents.
         (in-swapped?   (memq %swap-package (operating-system-packages swapped-os)))
         (in-default?   (memq %swap-package (operating-system-packages default-os)))
         (drives?       (and in-swapped? (not in-default?))))

    (format #t "~%== M6 differential: manifest-driven OCI image swap ==~%")
    (format #t "  oracle (system td)            : ~a~%" oracle)
    (format #t "  default manifest (base pkgs)  : ~a~%" default)
    (format #t "  swapped manifest (+hello)     : ~a~%" swapped)
    (format #t "~%  (a) converge   (default == oracle)        : ~a~%" converge?)
    (format #t "  (b) swap discriminates (swapped != oracle): ~a~%" discriminate?)
    (format #t "  (c) manifest drives contents              : ~a~%" drives?)
    (format #t "        hello in swapped pkgs : ~a   hello in default pkgs : ~a~%~%"
            (and in-swapped? #t) (and in-default? #t))

    (cond
     ((not converge?)
      (format #t "FAIL: the default manifest does not reproduce the oracle's OCI \
image derivation — the manifest layer is not purely additive.~%")
      (exit 1))
     ((not discriminate?)
      (format #t "FAIL: differential is vacuous — a swapped manifest (+hello) did \
NOT change the OCI image derivation. The oracle has lost discriminating power.~%")
      (exit 1))
     ((not drives?)
      (format #t "FAIL: the manifest does not drive image contents — the added \
package is not in the swapped system's package set (or leaked into the default).~%")
      (exit 1))
     (else
      (format #t "PASS: the default manifest is store-path-identical to the \
oracle; a swapped manifest yields a DIFFERENT image generation; and the declared \
manifest — only the manifest — decides what the image contains.~%")
      (exit 0)))))
