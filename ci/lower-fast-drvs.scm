;; ci/lower-fast-drvs.scm — lower every SYSTEM and OCI-image derivation the
;; fast tier (Makefile `check-fast`: the cheap rungs + `ts`) realises, one
;; /gnu/store/*.drv per line on stdout. Feeds the SMALL fast-tier CI store
;; image (ci/build-ci-image.sh TD_TIER=fast): the image must carry the build
;; closure of exactly what the cheap rungs lower, or the hosted runner's
;; offline ./check.sh check-fast reds on a missing input.
;;
;; The cheap rungs only LOWER (derivation-file-name of operating-system /
;; OCI-image objects with #:use-substitutes? #f) — they never build-derivations
;; — but lowering forces GRAFTS, so the grafted closures of each variant must
;; be present offline. This script lowers the SAME objects those rungs do:
;;   tests/typed-diff.scm        td-system, default, ssh-port 2222
;;   tests/typed-coverage.scm    every VALID field perturbation (the invalid
;;                               ones raise at construction — no derivation)
;;   tests/generation-diff.scm   generations 1 and 2 (sealed/verity shape)
;;   tests/manifest-diff.scm     OCI of default, swapped (+hello), empty manifest
;;
;; DRIFT: if a rung gains a VALID perturbation that pulls NEW closure (a new
;; package field, a new sealed shape), add it here too. The completeness check
;; is the offline `./check.sh check-fast` run against the imported image — it
;; reds on any input this list missed. Keep this aligned with those scripts.
(use-modules (guix)
             (guix store)
             (guix derivations)
             (guix monads)
             (gnu)
             (gnu system)
             (gnu system image)
             (gnu packages base)        ;hello
             (srfi srfi-1)
             (system td)
             (system td-typed))

(define %swapped-manifest (cons hello %base-packages))

;; The operating-systems the cheap rungs lower to a SYSTEM derivation
;; (operating-system-derivation). Mirrors typed-diff / typed-coverage /
;; generation-diff.
(define %systems
  (list
   td-system
   (td-config->operating-system (td-config))
   (td-config->operating-system (td-config #:ssh-port 2222))
   ;; typed-coverage valid field perturbations
   (td-config->operating-system (td-config #:host-name "other-host"))
   (td-config->operating-system (td-config #:timezone "Europe/Paris"))
   (td-config->operating-system (td-config #:locale "fr_FR.utf8"))
   (td-config->operating-system (td-config #:root-fs-label "other-root"))
   (td-config->operating-system (td-config #:ssh-password-auth? #t))
   (td-config->operating-system (td-config #:ssh-challenge-response? #t))
   (td-config->operating-system (td-config #:manifest %swapped-manifest))
   (td-config->operating-system (td-config #:ship-guix? #t))
   (td-config->operating-system (td-config #:generation 1))
   ;; generation-diff: two sealed generations (verity/dm-verity closure)
   (td-config->operating-system (td-config #:generation 2))))

;; NOTE: typed-coverage's STRUCTURAL-WIRING perturbations (bootloader-target,
;; persistent-paths, root-mount, root-fs-type) are checked as PREDICATES on the
;; operating-system record — they are NOT lowered to a derivation (and e.g.
;; root-mount "/altroot" is not lowerable at all: "missing root file system").
;; So they pull NO build closure and are deliberately absent here.

;; The operating-systems the cheap rungs lower to an OCI-IMAGE derivation.
;; Mirrors manifest-diff (the OCI slice of workstream C retired the oci-diff
;; differential; the plain OCI image is now td-native — mk/gates/120-oci.mk).
(define %oci-systems
  (list
   td-system
   (td-config->operating-system (td-config))
   (td-config->operating-system (td-config #:ssh-port 2222))
   (td-config->operating-system (td-config #:manifest %swapped-manifest))
   (td-config->operating-system (td-config #:manifest '()))))

(with-store store
  ;; Match the rungs' build options exactly (offline lowering, no offload).
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (define (emit drv)
    (format #t "~a~%" (derivation-file-name drv)))
  (for-each (lambda (os)
              (emit (run-with-store store (operating-system-derivation os))))
            %systems)
  (for-each (lambda (os)
              (emit (run-with-store store
                      (lower-object
                       (system-image (image-with-os docker-image os))))))
            %oci-systems))
