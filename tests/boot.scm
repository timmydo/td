;; tests/boot.scm — the td TEST operating-systems + their qcow2 images (config
;; layer) for the td-NATIVE VM-boot harness (gates boot-disk-native /
;; reset-native; tests/vm-lib.sh + tests/boot-native.sh / reset-native.sh).
;;
;; %test-os is the shipped `td-system` plus an unprivileged login account and the
;; committed test SSH key authorized for it; %native-disk-image is its qcow2 (the
;; boot suite); %reset-os/%native-reset-image are a root-login non-volatile
;; variant (the CoW-reset suite). The marionette (gnu tests) boot/reset tests that
;; used to live here were RETIRED in favour of the SSH harness (move-off-Guile §5,
;; lever 3): no more (gnu tests) / (gnu build marionette) / marionette-eval, and
;; the harness boots the UN-instrumented image (no backdoor service) — closer to
;; the shipped qcow2. The gates lower these image defs to a derivation INLINE
;; (a `guix repl` one-liner, like the old realise-system-test macro), so this is
;; the SINGLE Guile file the harness needs — no per-image lowering shims. The
;; SHIPPED td-system/qcow2 oracle stays a different, un-overlaid object. Image
;; build still uses guix `system image` (config/toolchain layer, retired last §5).
(define-module (tests boot)
  #:use-module (gnu system)
  #:use-module (gnu system shadow)
  #:use-module (gnu services)
  #:use-module (gnu services ssh)
  #:use-module (gnu image)
  #:use-module (gnu system image)
  #:use-module (guix gexp)
  #:use-module (guix packages)
  #:use-module (system td)
  #:export (%test-os
            %expected-kernel-release
            %native-disk-image
            %native-reset-image))

(define %expected-kernel-release
  ;; linux-libre reports `uname -r` as "<version>-gnu".
  (string-append (package-version (operating-system-kernel td-system))
                 "-gnu"))

(define %test-user "tester")

(define %test-os
  ;; A TEST-ONLY overlay on the frozen `td-system`: it adds an unprivileged
  ;; login account and authorizes the committed test public key for it. The
  ;; shipped `td-system` (and the qcow2/OCI images that the M4/M5 differentials
  ;; pin as the oracle) stays untouched — we must not ship this account or key.
  ;; `(inherit config)` on the openssh service preserves the M3 hardening
  ;; (password auth off, root login off), so the positive login below is forced
  ;; through publickey as the non-root %test-user.
  (operating-system
    (inherit td-system)
    (users (cons (user-account
                  (name %test-user)
                  (group "users")
                  (comment "td boot-test login user")
                  (home-directory (string-append "/home/" %test-user)))
                 (operating-system-users td-system)))
    (services
     (cons
      ;; The disk-boot test boots a STANDALONE qcow2 (no shared host store —
      ;; unlike the old direct-kernel `(virtual-machine os)`), so the test
      ;; private key's /gnu/store path is ABSENT in the guest. Bake it into the
      ;; image at /td_test_key (0600, ssh-usable) at activation so the
      ;; in-guest ssh client (the M3+ key-login positive control) can use it.
      (simple-service 'td-test-privkey activation-service-type
        #~(begin
            (copy-file #$(local-file "keys/td_test_ed25519") "/td_test_key")
            (chmod "/td_test_key" #o600)))
      (modify-services (operating-system-user-services td-system)
        (openssh-service-type config =>
          (openssh-configuration
           (inherit config)
           (authorized-keys
            (list (list %test-user
                        (local-file "keys/td_test_ed25519.pub")))))))))))

;; The boot suite's image: the UN-instrumented %test-os as a qcow2 (no marionette
;; backdoor), built exactly as `guix system image -t qcow2`.
(define %native-disk-image
  (system-image ((image-type-constructor qcow2-image-type) %test-os)))

;; The reset suite needs to WRITE a tiny dirt file and check its persistence. The
;; guix root fs is sized tight (≈full); a non-root user hits ENOSPC, but root can
;; use ext4's reserved blocks — exactly what the marionette test relied on (it ran
;; in-guest as root). So this TEST image authorizes the test key for ROOT and
;; permits key-only root login (overriding %test-os's M3 hardening — it tests CoW
;; ephemerality, not ssh hardening, and is never shipped).
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

;; Non-volatile (volatile-root? #f) so guest writes land on the qcow2 overlay and
;; genuinely PERSIST when an overlay is reused — the strict case the CoW-reset
;; ephemerality assertion needs.
(define %native-reset-image
  (system-image
   (image
    (inherit ((image-type-constructor qcow2-image-type) %reset-os))
    (volatile-root? #f))))
