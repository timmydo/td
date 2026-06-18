;; tests/reset-native-drv.scm — emit the NON-volatile qcow2 derivation for the
;; td-native reset test (move-off-Guile §5, lever 3). Prints `DRV=<path>` for a
;; qcow2 of %test-os with volatile-root? #f, so guest writes land on the qcow2
;; overlay (not an in-RAM volatile root) and genuinely PERSIST when an overlay is
;; reused — the strict case the CoW-reset ephemerality assertion needs (see
;; tests/reset.scm for why the stock volatile image cannot work here). The native
;; reset harness (tests/reset-native.sh) boots this over explicit CoW overlays and
;; asserts ephemerality over SSH — no marionette, no Guile in the guest. Un-
;; instrumented (no marionette backdoor); image build is the config/toolchain
;; layer (retired last, §5), not the test harness lever 3 targets.
(use-modules (guix)
             (guix gexp)
             (gnu image)
             (gnu system)
             (gnu services)
             (gnu services ssh)
             (gnu system image)
             (tests boot))             ;%test-os

;; The reset test writes a tiny dirt file and checks its persistence/absence. The
;; guix root fs is sized tight (≈full); a NON-root user hits ENOSPC, but root can
;; use ext4's reserved blocks — exactly what the marionette test relied on (it
;; ran in-guest as root). So this TEST image authorizes the committed test key for
;; ROOT and permits key-only root login (overriding %test-os's M3 hardening — this
;; image tests CoW ephemerality, not ssh hardening, and is never shipped). The
;; native harness then logs in as root, as the marionette did.
(define %reset-os
  (operating-system
    (inherit %test-os)
    (services
     (modify-services (operating-system-user-services %test-os)
       (openssh-service-type config =>
         (openssh-configuration
          (inherit config)
          (permit-root-login 'prohibit-password)
          (authorized-keys
           (list (list "root" (local-file "keys/td_test_ed25519.pub"))))))))))

(define %native-reset-image
  (system-image
   (image
    (inherit ((image-type-constructor qcow2-image-type) %reset-os))
    (volatile-root? #f))))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (format #t "DRV=~a~%"
          (derivation-file-name
           (run-with-store store (lower-object %native-reset-image)))))
