;; tests/boot.scm — the td TEST operating-system overlay (config layer).
;;
;; %test-os is the shipped `td-system` plus an unprivileged login account and the
;; committed test SSH key authorized for it — the system the td-NATIVE VM-boot
;; harness boots and drives over SSH (gates boot-disk-native / reset-native;
;; tests/vm-lib.sh + tests/boot-native.sh / reset-native.sh). The marionette
;; (gnu tests) boot/reset tests that used to live here were RETIRED in favour of
;; that harness (move-off-Guile §5, lever 3): no more (gnu tests) / (gnu build
;; marionette) / marionette-eval, and the native harness boots the UN-instrumented
;; image (no backdoor service) — closer to the shipped qcow2, as the old residual
;; note here anticipated. This module now carries only the test-OS overlay and the
;; declared kernel release the harness's M1 asserts. The SHIPPED td-system/qcow2
;; oracle stays a different, un-overlaid object (untouched). Image build still uses
;; guix `system image` (config/toolchain layer, retired last §5).
(define-module (tests boot)
  #:use-module (gnu system)
  #:use-module (gnu system shadow)
  #:use-module (gnu services)
  #:use-module (gnu services ssh)
  #:use-module (guix gexp)
  #:use-module (guix packages)
  #:use-module (system td)
  #:export (%test-os
            %expected-kernel-release))

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
