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
             (gnu system image)
             (tests boot))             ;%test-os

(define %native-reset-image
  (system-image
   (image
    (inherit ((image-type-constructor qcow2-image-type) %test-os))
    (volatile-root? #f))))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (format #t "DRV=~a~%"
          (derivation-file-name
           (run-with-store store (lower-object %native-reset-image)))))
