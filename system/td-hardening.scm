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
;;   * Round 4: that helper was bypassable — the public
;;     `td-config->operating-system` still lowered an UNGATED image, so
;;     "by construction" was false for any caller using bare `guix system image`.
;;     Fixed by EMBEDDING `guix-free-marker` in the hardened system's package set,
;;     so every lowering builds the profile and therefore the marker.
;;   * Round 5 (review F1): the embedded marker is NOT a whole-system gate. It only
;;     mounts the closure of the PACKAGES it is handed (the manifest), so its `ftw`
;;     walk over the sandbox's restricted /gnu/store sees only MANIFEST-injected
;;     guix. Guix injected by a SERVICE (e.g. `guix-service-type`) lives in the
;;     system closure but NOT in `operating-system-packages`, so the marker never
;;     sees it: a regression that drops `(delete guix-service-type)` ships guix yet
;;     the marker still passes (it was enforced only by differential convergence
;;     with the oracle, not by the gate). Reproduced: restoring guix-service-type to
;;     a ship-guix? #f system built fine and shipped 4 guix binaries.
;;
;; Two-layer guarantee as of Round 5:
;;   1. `guix-free-marker` — embedded in `packages`, so EVERY lowering builds it.
;;      It is a cheap, manifest-level PRE-FILTER: it catches guix reaching the
;;      closure via a manifest package (directly, propagated, a runtime reference,
;;      or a renamed/inherited package — things a static name scan misses). It does
;;      NOT and CANNOT catch service-injected guix (the marker cannot reference the
;;      whole system without a circular dependency on itself).
;;   2. `guix-free-system-gate` — the real WHOLE-SYSTEM gate. It references
;;      `operating-system-derivation`, whose closure is the entire folded system
;;      (profile + services + boot + activation), and fails the build if any
;;      bin/guix is in it. This catches service-injected guix. The Makefile's
;;      `no-guix` rung builds this gate over the SHIPPED system (so a guix-service
;;      regression in system/td.scm reddens) and over a service-injection
;;      adversarial fixture (verified-RED: it MUST fail at the gate diagnostic).
;;
;; The constructor's name/propagation check (in (system td-typed)) remains only a
;; sub-second fast-fail for the obvious `(list guix)` mistake.
;;
;; This module deliberately does NOT import (system td-typed): td-typed imports it
;; (to embed the marker), so the dependency must point one way only.
(define-module (system td-hardening)
  #:use-module (guix packages)
  #:use-module (guix gexp)
  #:use-module (guix store)              ;%store-monad
  #:use-module (guix monads)             ;mlet
  #:use-module (guix build-system trivial)
  #:use-module ((guix licenses) #:prefix license:)
  #:use-module (gnu services)            ;simple-service, activation-service-type
  #:use-module (gnu system)              ;operating-system-derivation
  #:use-module (gnu system file-systems) ;file-system (cgroup2 host mount)
  #:export (guix-free-marker
            guix-free-system-gate
            guix-free-privsep-service
            cgroup2-file-system))

;; M9 — a container host must expose a cgroup2 hierarchy. crun (and every OCI
;; runtime) probes /sys/fs/cgroup at startup and ABORTS if it is not a
;; cgroup/cgroup2 mount ("invalid file system type on /sys/fs/cgroup"). The
;; minimal td base mounts /sys (sysfs) but nothing mounts cgroup2 over it (no
;; elogind/systemd in this minimal system), so without this the booted base
;; cannot host containers — verified by the M9 feasibility gate, where a manual
;; `mount -t cgroup2 none /sys/fs/cgroup` was exactly what made crun run. This
;; mounts it declaratively at boot. Added to the file-systems of BOTH the oracle
;; (system td) and the typed compiler (td-config->operating-system), identically,
;; so the M4/M5/M6 differentials keep converging (cf. guix-free-privsep-service).
;; create-mount-point? #f — the directory already exists inside the sysfs /sys
;; mount; needed-for-boot? #f — cgroups are only needed once we run containers,
;; well after the root filesystem is up.
(define cgroup2-file-system
  (file-system
    (device "none")
    (mount-point "/sys/fs/cgroup")
    (type "cgroup2")
    (check? #f)
    (create-mount-point? #f)
    (needed-for-boot? #f)))

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
marker. Referencing PACKAGES in the builder forces the build sandbox to mount their
full closures under /gnu/store, so the `ftw` walk sees exactly the MANIFEST's
closure (verified: a referenced guix yields two `bin/guix*` hits).

SCOPE (review F1): this is a MANIFEST-LEVEL PRE-FILTER, not a whole-system gate. It
only sees the packages it is handed; guix injected by a SERVICE (e.g.
guix-service-type) is in the system closure but not in this package set, so the
marker cannot see it. The whole-system guarantee is `guix-free-system-gate`."
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
    (synopsis "Build-time guix-free PRE-FILTER embedded in hardened td systems")
    (description "A zero-content package whose build FAILS if any `bin/guix` or
`bin/guix-daemon` is present in the closure of the MANIFEST packages it is given.
Embedded in a ship-guix? #f system's package list so that building the system — by
any lowering — refuses to proceed if guix reaches the closure via a manifest
package. NOTE: this is a manifest-level pre-filter; the whole-system guarantee
(catching service-injected guix) is `guix-free-system-gate`.")
    (home-page "https://example.invalid/td-guix-free-marker")
    (license license:gpl3+)))

;; The real WHOLE-SYSTEM guix-free gate (review F1). Where `guix-free-marker` only
;; sees the manifest, this references the ENTIRE folded system closure via
;; `operating-system-derivation` — so it catches guix injected by a SERVICE
;; (guix-service-type's guix-daemon, build users, etc.), which lives in the system
;; closure but never in `operating-system-packages`. It cannot be embedded in the
;; system itself (it would reference the system that contains it — circular), so it
;; is applied as a separate gate derivation by the `no-guix` rung: over the SHIPPED
;; system (a guix-service regression in system/td.scm reddens) and over a
;; service-injection adversarial fixture (which MUST fail at the diagnostic below).
(define (guix-free-system-gate os)
  "Return a store-monad value yielding a <derivation> that builds to an empty
output IFF the FULL system closure of OS contains no `bin/guix`/`bin/guix-daemon`;
otherwise its build FAILS with a clear diagnostic. Ungexping the system derivation
mounts its entire closure under the sandbox's restricted /gnu/store, so the `ftw`
walk sees exactly the system closure — including packages contributed by SERVICES,
which `guix-free-marker` (manifest-only) cannot see."
  (mlet %store-monad ((sys (operating-system-derivation os)))
    (gexp->derivation
     "td-guix-free-system-gate"
     #~(begin
         (use-modules (ice-9 ftw))
         ;; Ungexp the system derivation: this records it as a build input, so guix
         ;; mounts its full closure (all requisites) under /gnu/store here. The
         ;; sandbox store view is restricted to exactly these inputs, so the walk
         ;; below sees precisely the system closure — nothing more, nothing less.
         (define system #$sys)
         (define (guix-binary? path)
           (and (member (basename path) '("guix" "guix-daemon"))
                (string=? (basename (dirname path)) "bin")))
         (define hits '())
         (ftw "/gnu/store"
              (lambda (filename statinfo flag)
                (when (and (eq? flag 'regular) (guix-binary? filename))
                  (set! hits (cons filename hits)))
                #t))
         ;; Keep the reference live (the input matters, not this value).
         (when (and #f (string? system)) #t)
         (if (null? hits)
             (mkdir #$output)
             (begin
               (format (current-error-port)
                       "td hardening: ship-guix? #f SYSTEM CLOSURE still contains \
guix:~%~{  ~a~%~}" hits)
               (error "td hardening: system closure STILL contains a \
guix/guix-daemon binary — a service (e.g. guix-service-type) re-introduced the \
imperative surface")))))))
