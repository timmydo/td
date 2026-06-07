;; tests/manifest-diff.scm — M6 differential test (DESIGN.md §6: manifest-driven,
;; image-swap-only; §2.4 step 5+, §2.5).
;;
;; M6 makes the package contents of the image a declarative function of a
;; *manifest* (the typed config's `manifest` field): the intended way to change
;; what the image contains is to declare a different manifest and rebuild the
;; WHOLE image — a wholesale swap, not an in-place edit. NOTE (triage): this is a
;; BUILD-INTERFACE property; M6 by itself did NOT remove the imperative `guix
;; install` surface — that is M7's `ship-guix?`, now the shipped default, so the
;; default image is guix-free (see `make no-guix`). This differential deliberately
;; isolates the M6 manifest-swap mechanism and is independent of that flag: both
;; sides default `ship-guix?` identically, so guix-free-ness is held constant and
;; only the manifest varies. The artifact under test is the OCI image derivation
;; (the thing you would swap), exactly as in M5.
;;
;; The package contract (made precise by triage F-review #2): a td system's
;; EFFECTIVE package set is
;;
;;     effective packages = fixed base capabilities   ; crun — container host
;;                        + manifest-selected payload  ; the td-config manifest
;;                        + enforcement markers         ; guix-free-marker (#f)
;;
;; The manifest controls ONLY the swappable payload — it is NOT the whole
;; package set. Changing the manifest produces a new image generation (b); it
;; cannot add or remove a base capability (d). Earlier wording ("the manifest IS
;; the package set", "only the manifest decides contents") overstated this and is
;; corrected here: the base capability `crun` is a mandatory platform invariant
;; the compiler injects regardless of (and absent from) the manifest.
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
;;   (c) MANIFEST DRIVES THE SWAPPABLE PAYLOAD — the added package is in the
;;       swapped system's package set (operating-system-packages) and ABSENT
;;       from BOTH the default's and an empty-manifest system's. This proves
;;       (b)'s divergence is the manifest doing real work on the payload, not an
;;       incidental hash change. NOTE (triage #5): this is a DECLARATION-level
;;       check — it does not prove the package's files reach the realized
;;       tarball (an exporter bug could drop them). That stronger artifact-level
;;       claim is `make manifest-check`, which cracks the built layer.tar and
;;       asserts hello/bin/hello is actually packed.
;;   (d) BASE CAPABILITY INVARIANT — `crun` (the container-host capability) is
;;       in EVERY system's package set — default, swapped, AND empty-manifest —
;;       yet is in NONE of the supplied manifests. The base capability is a
;;       mandatory platform invariant the compiler injects, not swappable
;;       manifest content: the manifest neither added it nor can remove it. This
;;       is what makes the contract above precise (effective = base + payload +
;;       markers) rather than the overstated "the manifest IS the package set".
;;
;; (a)+(b) are the load-bearing self-discriminating pair (break the compiler's
;; default → (a) red; make the swap a no-op → (b) red). (c) pins the divergence
;; to the manifest-selected payload; (d) pins the base capability OUTSIDE the
;; manifest. The bit-for-bit reproducibility of a SWAPPED generation AND its
;; realized contents are proven separately by `make manifest-check`.
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
             (gnu packages containers)  ;crun — the base capability (d)
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
  ;; Offline contract (triage): forbid substitutes AND remote offloading for this
  ;; store session. The shared host daemon has network + a nonguix substitute URL
  ;; (check.sh), and `guix repl` does not read GUIX_BUILD_OPTIONS — so set both
  ;; here. This guarantees no binary substitutes and no remote builders; a cold
  ;; fixed-output SOURCE fetch by the shared daemon is still possible (the narrowed
  ;; contract — see check.sh / DESIGN §5). #:use-substitutes? #f also stops the
  ;; graft-driven substitute *queries* this rung's first run made when `hello` was
  ;; not yet warm.
  (set-build-options store #:use-substitutes? #f #:offload? #f)

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
  ;; An EMPTY-manifest system — the payload is gone, but the base capability
  ;; must remain. Used by (c)/(d) to pin crun to the base, not the manifest.
  (define empty-os     (td-config->operating-system
                        (td-config #:manifest '())))

  (let* ((oracle  (oci-drv td-system))
         (default (oci-drv default-os))
         (swapped (oci-drv swapped-os))
         (converge?     (string=? oracle default))
         (discriminate? (not (string=? oracle swapped)))
         ;; (c): the manifest drives the swappable PAYLOAD — hello appears only
         ;; in the swapped system, never in the default or empty-manifest ones.
         (in-swapped?   (memq %swap-package (operating-system-packages swapped-os)))
         (in-default?   (memq %swap-package (operating-system-packages default-os)))
         (in-empty?     (memq %swap-package (operating-system-packages empty-os)))
         (drives?       (and in-swapped? (not in-default?) (not in-empty?)))
         ;; (d): the BASE capability invariant — crun is injected by the compiler
         ;; into every system regardless of the manifest, and is in NONE of the
         ;; supplied manifests. The manifest neither added it nor can remove it.
         (crun-in-manifests? (or (memq crun %base-packages)
                                 (memq crun %swapped-manifest)))
         (crun-default? (memq crun (operating-system-packages default-os)))
         (crun-swapped? (memq crun (operating-system-packages swapped-os)))
         (crun-empty?   (memq crun (operating-system-packages empty-os)))
         (base-invariant? (and (not crun-in-manifests?)
                               crun-default? crun-swapped? crun-empty?)))

    (format #t "~%== M6 differential: manifest-driven OCI image swap ==~%")
    (format #t "  oracle (system td)            : ~a~%" oracle)
    (format #t "  default manifest (base pkgs)  : ~a~%" default)
    (format #t "  swapped manifest (+hello)     : ~a~%" swapped)
    (format #t "~%  (a) converge   (default == oracle)        : ~a~%" converge?)
    (format #t "  (b) swap discriminates (swapped != oracle): ~a~%" discriminate?)
    (format #t "  (c) manifest drives swappable payload     : ~a~%" drives?)
    (format #t "        hello in pkgs — swapped:~a default:~a empty:~a~%"
            (and in-swapped? #t) (and in-default? #t) (and in-empty? #t))
    (format #t "  (d) base capability invariant (crun)      : ~a~%" (and base-invariant? #t))
    (format #t "        crun in pkgs — default:~a swapped:~a empty:~a   in manifests:~a~%~%"
            (and crun-default? #t) (and crun-swapped? #t) (and crun-empty? #t)
            (and crun-in-manifests? #t))

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
      (format #t "FAIL: the manifest does not drive the swappable payload — the \
added package is not in the swapped system's packages (or leaked into the default \
or empty-manifest system).~%")
      (exit 1))
     ((not base-invariant?)
      (format #t "FAIL: the base capability invariant is broken — crun must be in \
every system's packages (default/swapped/empty) and in none of the supplied \
manifests. Either crun leaked into a manifest or a manifest could drop it; the \
'effective = base + payload + markers' contract no longer holds.~%")
      (exit 1))
     (else
      (format #t "PASS: the default manifest is store-path-identical to the \
oracle; a swapped manifest yields a DIFFERENT image generation; the manifest \
drives the swappable payload (hello); and the base capability crun is a manifest- \
independent invariant (artifact-level presence in the realized tarball is proven \
by `make manifest-check`).~%")
      (exit 0)))))
