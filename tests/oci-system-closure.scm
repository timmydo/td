;; tests/oci-system-closure.scm — INPUT RESOLUTION for the td-native OCI system image
;; (the OCI slice of workstream C: "retire the Guile lowering"). This does NOT construct
;; the image. It only RESOLVES which store paths form the td system's runtime closure —
;; the retired-last, stays-Guix half (DESIGN §5). td-builder packs those paths into the
;; docker-archive itself (`td-builder oci-image-paths`, builder/src/oci.rs); the image
;; CONSTRUCTION is td-native, replacing guix's `(gnu system image)` docker lowering.
;;
;; Deliberately reads NO guix private state and calls no image lowering: it realizes the
;; frozen oracle `td-system` exactly as `guix system build` does (operating-system-
;; derivation) and prints its `requisites` closure, one store path per line, for the gate
;; to feed to td-builder. So the residual guix here is pure input resolution (the
;; `lowering` census category, retired last) — the guix surface only shrinks.
;;
;; Run as a repl SCRIPT (not piped via STDIN) — see tests/typed-diff.scm.
(use-modules (guix store)
             (guix derivations)
             (guix monads)
             (gnu)
             (gnu system)
             (system td))

(with-store store
  ;; Offline contract (triage): no substitutes, no remote offloading (guix repl
  ;; ignores GUIX_BUILD_OPTIONS) — see tests/typed-diff.scm / check.sh.
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (let* ((drv (run-with-store store (operating-system-derivation td-system)))
         (out (derivation->output-path drv)))
    ;; Realize the system so its closure is valid in the store, then emit the closure.
    (build-derivations store (list drv))
    (for-each (lambda (p) (display p) (newline))
              (requisites store (list out)))))
