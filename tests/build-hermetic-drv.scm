;; tests/build-hermetic-drv.scm — emit a PROBE derivation for the build-hermetic
;; gate (mk/gates/356, own-builder-daemon increment 2). The probe's builder FAILS
;; the build if a host path the outer loop-sandbox exposes — but a hermetic build
;; must NEVER see — is reachable inside td's build sandbox: /var/guix (the guix
;; daemon db/socket/gc-roots, bound read-write into the loop container). So
;; `td-builder realize` of this drv succeeds ONLY because sandbox::build pivot_roots
;; into a minimal root that drops the invoking filesystem.
;;
;; The daemon builds it here to realize the guile closure inputs and prove the probe
;; is well-formed; the daemon's own build chroot has no /var/guix, so it passes too
;; — the discriminating environment is td's sandbox, exercised by the gate's realize.
;;
;; Emits: PROBE_DRV (the .drv file name), PROBE_OUT (its output path).
(use-modules (guix) (guix gexp) (guix monads) (guix derivations))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let ((drv (run-with-store store
               (gexp->derivation "td-build-hermetic-probe"
                 #~(begin
                     ;; A hermetic build must not reach the guix daemon state.
                     (when (file-exists? "/var/guix")
                       (error "LEAK: /var/guix reachable inside td's build sandbox"))
                     ;; Sanity: the staged store IS present (the build is not empty).
                     (unless (file-exists? "/gnu/store")
                       (error "no /gnu/store in td's build sandbox"))
                     (mkdir #$output))))))
    (build-derivations store (list drv))
    (format #t "PROBE_DRV=~a~%" (derivation-file-name drv))
    (format #t "PROBE_OUT=~a~%" (derivation->output-path drv))))
