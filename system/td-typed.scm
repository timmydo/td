;; system/td-typed.scm — M4: a typed config front-end that compiles to gexps.
;;
;; DESIGN.md §2.4 milestone 4: "Introduce the typed config front-end that
;; compiles to gexps; differential test: compiled output yields the same store
;; paths as the hand-written gexp." The hand-written `td-system` in
;; (system td) stays FROZEN as the oracle (DESIGN §2.5); this module is an
;; INDEPENDENT second construction of the same system from a small, *typed*
;; description. `tests/typed-diff.scm` proves the two converge to the same
;; system derivation (and that a perturbed config diverges).
;;
;; "Typed" here = a schema with teeth: `td-config` is a smart constructor that
;; validates every field's type/range and raises on violation, so a malformed
;; configuration is rejected at construction rather than producing a subtly
;; wrong system. This is the v0 stand-in for the eventual typed front-end; it
;; is hand-rolled (not `define-configuration`) so the lowering to an
;; operating-system is fully under our control and can be kept byte-identical
;; to the oracle — the whole point of the differential.
(define-module (system td-typed)
  #:use-module (gnu)                     ;operating-system, modify-services, delete
  #:use-module (gnu bootloader grub)
  #:use-module (gnu services base)       ;guix-service-type
  #:use-module (gnu services networking)
  #:use-module (gnu services ssh)
  #:use-module (gnu system file-systems)
  #:use-module (guix packages)
  #:use-module (system td-hardening)     ;guix-free-marker (embedded build gate)
  #:use-module (srfi srfi-1)
  #:use-module (srfi srfi-9)
  #:use-module (ice-9 match)
  #:use-module (ice-9 format)
  #:export (<td-config>
            td-config
            td-config?
            td-config-host-name
            td-config-timezone
            td-config-locale
            td-config-bootloader-target
            td-config-root-fs-label
            td-config-root-mount
            td-config-root-fs-type
            td-config-ssh-port
            td-config-ssh-password-auth?
            td-config-ssh-challenge-response?
            td-config-manifest
            td-config-ship-guix?
            td-config->operating-system
            %td-default-config))

;;;
;;; The typed record.
;;;

(define-record-type <td-config>
  (make-td-config host-name timezone locale bootloader-target
                  root-fs-label root-mount root-fs-type
                  ssh-port ssh-password-auth? ssh-challenge-response?
                  manifest ship-guix?)
  td-config?
  (host-name              td-config-host-name)
  (timezone               td-config-timezone)
  (locale                 td-config-locale)
  (bootloader-target      td-config-bootloader-target)
  (root-fs-label          td-config-root-fs-label)
  (root-mount             td-config-root-mount)
  (root-fs-type           td-config-root-fs-type)
  (ssh-port               td-config-ssh-port)
  (ssh-password-auth?     td-config-ssh-password-auth?)
  (ssh-challenge-response? td-config-ssh-challenge-response?)
  ;; M6 — the declarative package manifest that drives image contents. It is the
  ;; manifest-driven, image-swap-only BUILD INTERFACE (DESIGN §6): the intended way
  ;; to change what the image contains is to declare a different manifest and
  ;; rebuild the whole image — a wholesale swap, not an in-place edit of a built
  ;; image. NOTE (triage): this is an interface property only — M6 does NOT remove
  ;; the imperative `guix install` surface (the built image still ships
  ;; `guix`/`guix-daemon`); removing that surface is M7, via the `ship-guix?`
  ;; field below. A list of <package>; defaults to %base-packages so the default
  ;; config stays byte-identical to the frozen oracle (which lets the field
  ;; default).
  (manifest               td-config-manifest)
  ;; M7 — image-swap-only BY CONSTRUCTION (DESIGN §6 parking-lot, the documented
  ;; continuation of M6). M6 made image CONTENTS manifest-driven but left the
  ;; imperative mutation surface in place: the built image still ships
  ;; `guix`/`guix-daemon`, so an in-image `guix install` is physically possible.
  ;; This boolean is the lever that removes that surface: when #f the compiler
  ;; deletes `guix-service-type` (the service that pulls guix into the BASE system
  ;; closure) AND embeds the `guix-free-marker` in the system's package set. NOTE:
  ;; deleting the service is necessary but NOT sufficient on its own — a manifest
  ;; package can still drag guix into the closure (directly, propagated, via a
  ;; runtime reference, or as a renamed/inherited package). The constructor's
  ;; cross-field check below is only a CHEAP PRE-FILTER for the obvious cases; the
  ;; REAL, manifest-agnostic guix-free guarantee is the embedded closure-level BUILD
  ;; GATE (see (system td-hardening) `guix-free-marker`), which lives in `packages`
  ;; so EVERY lowering builds it — making a hardened image guix-free OR refuse to
  ;; build, with no opt-in path to bypass. Defaults to #t so the default config
  ;; stays byte-identical to the frozen oracle (§2.5); flipping the SHIPPED default
  ;; to #f re-baselines the oracle and is a spec decision gated on §4.3 sign-off —
  ;; M7 proves the construction additively, it does not unilaterally change what td
  ;; ships. `make no-guix` proves the guarantee end to end: the bare #f image is
  ;; guix-free (and reproducible), the #t image is not, and a manifest that smuggles
  ;; guix past the pre-filter is REFUSED at build time.
  (ship-guix?             td-config-ship-guix?))

;;;
;;; Validation — the "typed" guarantee. Each field is checked; a violation is a
;;; hard error at construction time, never a silently-wrong system.
;;;

(define (check pred field value expected)
  (unless (pred value)
    (error (format #f "td-config: field ~a: expected ~a, got: ~s"
                   field expected value))))

(define (non-empty-string? x)
  (and (string? x) (not (string-null? x))))

(define (absolute-path? x)
  (and (string? x) (string-prefix? "/" x)))

(define (tcp-port? x)
  (and (integer? x) (exact? x) (<= 1 x 65535)))

;; The filesystem types we know how to declare. Kept explicit so an unsupported
;; type is rejected here rather than failing deep in a build.
(define %known-fs-types '("ext4" "btrfs" "xfs"))

;; A manifest is a list of <package>. Validated structurally so a bad manifest
;; (a non-list, or a list with a non-package element) is rejected at
;; construction time rather than failing deep in an image build.
(define (package-list? x)
  (and (list? x) (every package? x)))

;; Every package a manifest installs into the image's PROFILE: each directly
;; listed package PLUS its transitively propagated inputs. Propagated inputs are
;; added to the profile (and so to the image) exactly as if they had been listed
;; directly — that is the mechanism by which a manifest package "propagating guix"
;; lands a `bin/guix` in the image even though guix is not itself in the list. We
;; flatten that closure here so the guix check below sees the same package set the
;; realized image's profile will. (Inputs can be non-package objects — origins,
;; file-likes — so we keep only the packages.)
(define (manifest-profile-packages manifest)
  (append manifest
          (filter-map (match-lambda
                        ((_ (? package? p) _ ...) p)
                        (_ #f))
                      (append-map package-transitive-propagated-inputs
                                  manifest))))

;; Does a manifest install the `guix` package — directly OR via a transitively
;; propagated input? Checked by NAME (not object identity) so a guix variant is
;; caught too. Used to reject the contradictory ship-guix? #f + guix-in-profile
;; combination — see the constructor.
(define (manifest-has-guix? manifest)
  (any (lambda (p) (string=? (package-name p) "guix"))
       (manifest-profile-packages manifest)))

;;;
;;; The smart constructor. Keyword-driven with defaults that, taken together,
;;; describe EXACTLY the system the hand-written `td-system` declares — so
;;; `%td-default-config` lowers to the oracle's store path.
;;;

(define* (td-config #:key
                    (host-name "td")
                    (timezone "UTC")
                    (locale "en_US.utf8")
                    (bootloader-target "/dev/vda")
                    (root-fs-label "td-root")
                    (root-mount "/")
                    (root-fs-type "ext4")
                    (ssh-port 22)
                    (ssh-password-auth? #f)
                    (ssh-challenge-response? #f)
                    (manifest %base-packages)
                    (ship-guix? #t))
  (check non-empty-string? 'host-name host-name "a non-empty string")
  (check non-empty-string? 'timezone timezone "a non-empty string")
  (check non-empty-string? 'locale locale "a non-empty string")
  (check absolute-path? 'bootloader-target bootloader-target
         "an absolute device path")
  (check non-empty-string? 'root-fs-label root-fs-label "a non-empty string")
  (check absolute-path? 'root-mount root-mount "an absolute mount path")
  (check (lambda (x) (member x %known-fs-types)) 'root-fs-type root-fs-type
         (format #f "one of ~a" %known-fs-types))
  (check tcp-port? 'ssh-port ssh-port "an integer in 1..65535")
  (check boolean? 'ssh-password-auth? ssh-password-auth? "a boolean")
  (check boolean? 'ssh-challenge-response? ssh-challenge-response? "a boolean")
  (check package-list? 'manifest manifest "a list of <package>")
  (check boolean? 'ship-guix? ship-guix? "a boolean")
  ;; Cross-field (M7) — CHEAP PRE-FILTER ONLY, not the guarantee. ship-guix? #f
  ;; promises an image with no imperative guix surface; the manifest can defeat that
  ;; by putting guix into the image's profile. We fast-fail the OBVIOUS cases here
  ;; (sub-second, before an expensive build): a manifest with guix listed directly
  ;; or via a transitively propagated input (`manifest-has-guix?` walks both). But
  ;; this is fundamentally incomplete — review showed guix can still reach the
  ;; closure as a NON-propagated runtime reference, or via a RENAMED package
  ;; inheriting guix (its name is not "guix"), neither of which a static name/
  ;; propagation scan can see. So this check is a convenience, NOT a guarantee. The
  ;; real, manifest-agnostic guarantee is the closure-level BUILD GATE embedded by
  ;; `td-config->operating-system` (the `guix-free-marker` from (system
  ;; td-hardening)), which scans the realized closure of the hardened profile and
  ;; fails the build if any bin/guix is present, so a hardened image is guix-free or
  ;; does not build, for ANY manifest and ANY lowering path. This pre-filter just
  ;; turns the common mistake into a fast, clear error before that build.
  (when (and (not ship-guix?) (manifest-has-guix? manifest))
    (error (string-append
            "td-config: ship-guix? #f is incompatible with a manifest that "
            "puts the `guix` package into the image profile (listed directly or "
            "via a transitively propagated input) — that would re-introduce the "
            "imperative `guix install` surface the flag removes. Drop guix from "
            "the manifest or set ship-guix? #t. (Note: this is only a pre-filter; "
            "the closure-level guarantee is the guix-free-marker embedded in the "
            "hardened system by td-config->operating-system — see (system td-hardening).)")))
  (make-td-config host-name timezone locale bootloader-target
                  root-fs-label root-mount root-fs-type
                  ssh-port ssh-password-auth? ssh-challenge-response?
                  manifest ship-guix?))

;;;
;;; The compiler: typed config -> operating-system (a gexp-bearing value).
;;; This mirrors (system td) field for field. Any drift here shows up as a
;;; store-path divergence in tests/typed-diff.scm — that is the test's job.
;;;

(define (td-config->operating-system c)
  (operating-system
    (host-name (td-config-host-name c))
    (timezone (td-config-timezone c))
    (locale (td-config-locale c))

    (bootloader
     (bootloader-configuration
      (bootloader grub-bootloader)
      (targets (list (td-config-bootloader-target c)))))

    (file-systems
     (cons (file-system
             (device (file-system-label (td-config-root-fs-label c)))
             (mount-point (td-config-root-mount c))
             (type (td-config-root-fs-type c)))
           %base-file-systems))

    ;; The declared manifest IS the package set of the image (M6). The default
    ;; manifest is %base-packages, which is exactly the operating-system field's
    ;; own default — so the default config lowers byte-for-byte to the frozen
    ;; oracle (which omits this field). A non-default manifest is a different
    ;; image: a whole-image swap, not an in-place install.
    ;;
    ;; M7 (F1, embedded gate): for a hardened (#f) config we ALSO prepend the
    ;; `guix-free-marker` — a build-time package whose build FAILS if guix is
    ;; anywhere in the (other) packages' closure. Because it lives in `packages`,
    ;; EVERY lowering of this system (bare operating-system, qcow2, docker, any
    ;; helper) builds the profile and therefore the marker, so a hardened image is
    ;; guix-free OR it does not build — by construction, with no bypassable opt-in.
    ;; For #t (the default/oracle) the package set is the manifest verbatim, so the
    ;; default config stays byte-identical to the frozen oracle (§2.5).
    (packages (let ((manifest (td-config-manifest c)))
                (if (td-config-ship-guix? c)
                    manifest
                    (cons (guix-free-marker manifest) manifest))))

    ;; M7: when `ship-guix?` is #f, delete `guix-service-type` so the realized
    ;; image carries no `guix`/`guix-daemon` binary — image-swap-only by
    ;; construction (no in-image `guix install`). When #t (the default) the
    ;; service list is exactly the frozen oracle's, so the default config stays
    ;; byte-identical (the M4/M5/M6 differentials keep converging).
    (services
     (let ((svcs (cons* (service dhcpcd-service-type)
                        (service openssh-service-type
                                 (openssh-configuration
                                  (port-number (td-config-ssh-port c))
                                  (password-authentication?
                                   (td-config-ssh-password-auth? c))
                                  (challenge-response-authentication?
                                   (td-config-ssh-challenge-response? c))))
                        %base-services)))
       (if (td-config-ship-guix? c)
           svcs
           (modify-services svcs (delete guix-service-type)))))))

;; The default typed config — by construction equal in content to `td-system`.
(define %td-default-config (td-config))
