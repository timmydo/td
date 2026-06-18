;; tests/boot-native-drv.scm — emit the qcow2 image derivation for the td-NATIVE
;; boot harness (move-off-Guile §5, lever 3). Prints `DRV=<path>` for the qcow2
;; of %test-os — the shipped td-system plus the test SSH user/key — WITHOUT the
;; marionette backdoor (no (marionette-operating-system …)): the native harness
;; boots this disk under qemu and asserts over SSH from the host (tests/vm-lib.sh
;; + tests/boot-native.sh), so no Guile runs in the guest and no (gnu tests) /
;; (gnu build marionette) is involved. This is the un-instrumented image the
;; boot.scm comment flagged as the follow-up ("a byte-exact boot … would need a
;; serial-console/ssh harness instead of the marionette"). The image build itself
;; still uses guix `system image` — that is the config/toolchain layer (retired
;; last, §5), not the test harness lever 3 targets.
(use-modules (guix)
             (guix gexp)
             (gnu system image)
             (gnu image)
             (system td)
             (tests boot))             ;%test-os (the test-user/key overlay)

(define %native-disk-image
  (system-image ((image-type-constructor qcow2-image-type) %test-os)))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (format #t "DRV=~a~%"
          (derivation-file-name
           (run-with-store store (lower-object %native-disk-image))))
  ;; The declared kernel release the boot suite's M1 asserts (no magic constant).
  (format #t "KERNEL=~a~%" %expected-kernel-release))
