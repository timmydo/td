;; system/td-hardening.scm — M7 triage F1: the guix-free guarantee, EMBEDDED.
;;
;; History (external review, three rounds):
;;   * Round 1: ship-guix? #f only deleted guix-service-type; a manifest LISTING
;;     guix still shipped it. Added a constructor name check.
;;   * Round 2: a manifest package PROPAGATING guix bypassed the name check.
;;     Extended the constructor to walk transitive propagated inputs.
;;   * Round 3: any STATIC check is incomplete — guix still reaches the closure via
;;     a plain RUNTIME REFERENCE or a RENAMED package inheriting guix. Added a
;;     closure-level build GATE… but only in an OPT-IN docker helper.
;;   * Round 4 (this module): that helper was bypassable — the public
;;     `td-config->operating-system` still lowered an UNGATED image, so
;;     "by construction" was false for any caller using bare `guix system image`.
;;
;; Fix: stop bolting the gate onto one lowering path. EMBED it in the hardened
;; operating-system's package set as `guix-free-marker`. Because it lives in
;; `packages`, EVERY lowering of that system — bare `operating-system`, qcow2,
;; docker, or any helper — builds the profile and therefore builds the marker, so a
;; hardened image is guix-free OR it does not build. No opt-in, no path a caller can
;; skip. The marker is closure-level and manifest-agnostic: it scans the realized
;; closure of the hardened profile and catches guix arriving via a runtime
;; reference or a renamed/inherited package, which a static name/propagation scan
;; cannot. The constructor's name/propagation check (in (system td-typed)) is kept
;; only as a cheap fast-fail PRE-FILTER for the obvious `(list guix)` mistake.
;;
;; This module deliberately does NOT import (system td-typed): td-typed imports it
;; (to embed the marker), so the dependency must point one way only.
(define-module (system td-hardening)
  #:use-module (guix packages)
  #:use-module (guix gexp)
  #:use-module (guix build-system trivial)
  #:use-module ((guix licenses) #:prefix license:)
  #:use-module (gnu services)            ;simple-service, activation-service-type
  #:export (guix-free-marker
            guix-free-privsep-service))

;; A guix-free system needs its sshd privilege-separation directory set up
;; explicitly. On a normal (guix-ful) Guix system, `guix-service-type` declares
;; the `guixbuilder` build users whose home is `/var/empty`, and creating those
;; accounts leaves `/var/empty` owned `root:root` mode 0755 — exactly what sshd's
;; privsep requires. Deleting `guix-service-type` (ship-guix? #f) removes that
;; side effect, so `/var/empty` ends up with perms sshd rejects ("must be owned by
;; root and not group or world-writable"), and every per-connection `sshd -i`
;; child aborts at startup. This activation snippet restores the invariant
;; directly, so a guix-free td system runs sshd. It is added to the #f path of
;; BOTH the hand-written oracle (system td) and the typed compiler
;; (td-config->operating-system), identically, so the M4/M5/M6 differentials keep
;; converging (cf. `guix-free-marker`). chown/chmod/mkdir/file-exists? are core
;; Guile, so the snippet needs no extra modules in the activation context.
(define guix-free-privsep-service
  (simple-service 'td-sshd-privsep activation-service-type
                  #~(begin
                      (unless (file-exists? "/var/empty")
                        (mkdir "/var/empty"))
                      (chown "/var/empty" 0 0)
                      (chmod "/var/empty" #o755))))

(define (guix-free-marker packages)
  "Return a trivial <package> that builds to an empty output IFF none of PACKAGES'
runtime closures contains a `bin/guix`/`bin/guix-daemon`; otherwise its build
FAILS with a clear diagnostic. Add it to a hardened (ship-guix? #f) system's
package list so EVERY lowering of that system builds the profile and therefore this
marker — making the guix-free property hold BY CONSTRUCTION for any lowering path,
not just an opt-in helper. Referencing PACKAGES in the builder forces the build
sandbox to mount their full closures under /gnu/store, so the `ftw` walk sees
exactly the hardened profile's closure (verified: a referenced guix yields two
`bin/guix*` hits)."
  (package
    (name "td-guix-free-marker")
    (version "0")
    (source #f)
    (build-system trivial-build-system)
    (arguments
     (list
      #:builder
      #~(begin
          (use-modules (ice-9 ftw))
          ;; Force PACKAGES into this derivation's closure: each ungexp records a
          ;; build input, so guix mounts their requisites under /gnu/store here.
          (define scanned (list #$@packages))
          (define (guix-binary? path)
            (and (member (basename path) '("guix" "guix-daemon"))
                 (string=? (basename (dirname path)) "bin")))
          (define hits '())
          (ftw "/gnu/store"
               (lambda (filename statinfo flag)
                 (when (and (eq? flag 'regular) (guix-binary? filename))
                   (set! hits (cons filename hits)))
                 #t))
          ;; Keep the reference live (the inputs matter, not this value).
          (when (and #f (pair? scanned)) #t)
          (if (null? hits)
              (mkdir #$output)
              (begin
                (format (current-error-port)
                        "td hardening: ship-guix? #f system closure STILL contains \
guix:~%~{  ~a~%~}" hits)
                (error "td hardening: ship-guix? #f system STILL contains a \
guix/guix-daemon binary in its closure — the imperative surface was not \
removed"))))))
    (synopsis "Build-time guix-free gate embedded in hardened td systems")
    (description "A zero-content package whose build FAILS if any `bin/guix` or
`bin/guix-daemon` is present in the closure of the packages it is given. Embedded
in a ship-guix? #f system's package list so that building the system — by any
lowering — refuses to proceed if guix is in the closure, making the guix-free
guarantee hold by construction rather than via an opt-in helper.")
    (home-page "https://example.invalid/td-guix-free-marker")
    (license license:gpl3+)))
