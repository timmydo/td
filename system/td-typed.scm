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
  #:use-module (gnu packages containers) ;crun — shipped in the base (M9)
  #:use-module (gnu packages rust-apps)  ;procs/fd/ripgrep/sd/eza/bat — Rust userland
  #:use-module (gnu packages ssh)        ;openssh — host-key generation (§2.6)
  #:use-module (guix gexp)
  #:use-module (guix packages)
  ;; guix-free-marker, guix-free-privsep-service, cgroup2-file-system
  #:use-module (system td-hardening)
  ;; %td-verity-target, td-verity-mapped-device (M11 — sealed generations)
  #:use-module (system td-verity)
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
            td-config-generation
            td-config-persistent-paths
            td-config-effective-root-label
            td-config->operating-system
            %td-default-config
            %td-default-persistent-paths
            %td-state-label
            %td-state-mount-point
            %td-ssh-host-key))

;;;
;;; The typed record.
;;;

(define-record-type <td-config>
  (make-td-config host-name timezone locale bootloader-target
                  root-fs-label root-mount root-fs-type
                  ssh-port ssh-password-auth? ssh-challenge-response?
                  manifest ship-guix? generation persistent-paths)
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
  ;; build, with no opt-in path to bypass. **Defaults to #f — the shipped default
  ;; (signed off 2026-06-06, DESIGN §4.3): td ships a guix-free, image-swap-only
  ;; distro BY CONSTRUCTION, VM and OCI image alike.** The frozen oracle
  ;; (system td) was re-baselined to match — it now embeds the same marker and
  ;; deletes guix-service-type — so the M4/M5/M6 differentials still converge, at
  ;; the new guix-free digests. (Convergence now also ENFORCES the gate on the
  ;; hand-written oracle: it cannot drop the marker without diverging.) `make
  ;; no-guix` proves the guarantee end to end on explicit fixtures: the #f image is
  ;; guix-free (and reproducible), an explicit #t image is not, and a manifest that
  ;; smuggles guix past the pre-filter is REFUSED at build time.
  (ship-guix?             td-config-ship-guix?)
  ;; M10.1 — the generation identifier. A "generation" is a bootc-style bootable
  ;; image you can place, list in GRUB, and roll back to (M10-design.md). Its
  ;; whole point is that each generation boots its OWN root, not the shared
  ;; `td-root`: otherwise every GRUB entry mounts the same filesystem and rollback
  ;; is a no-op (the P1 crux). So when this is a positive integer the compiler
  ;; derives a DISTINCT, bootloader-selectable root label (`<root>-gen-<n>`) for
  ;; this system; when #f (the default) the root stays the plain `root-fs-label`,
  ;; so the default config still lowers byte-identically to the frozen oracle.
  (generation             td-config-generation)
  ;; M10.3 — the DECLARED-PERSISTENCE allowlist (DESIGN §2.6, settled
  ;; 2026-06-10). Persistence on a td machine is default-deny: ONLY the paths
  ;; listed here survive a generation swap, each bind-mounted at boot from the
  ;; machine's single writable filesystem (label `td-state`). Entries are
  ;; (TIER . ABSOLUTE-PATH) pairs; TIER is 'precious (backed by td-state/state —
  ;; machine identity, backup-worthy) or 'disposable (td-state/cache —
  ;; persistent but re-derivable). The default carries the model's first entry:
  ;; /var/lib/ssh (precious), where the compiler relocates the SSH host key so a
  ;; rollback swaps the OS but never the machine identity. GENERATION-MODE ONLY
  ;; (§2.6 "oracle scope"): with generation #f nothing here is emitted, so the
  ;; default config still lowers byte-identically to the frozen oracle.
  (persistent-paths       td-config-persistent-paths))

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

;; A generation id is #f (no generation — the plain, shared-root system) or a
;; positive exact integer. Zero/negative/non-integer ids are rejected so a
;; malformed generation cannot derive a bogus root label.
(define (generation-id? x)
  (or (not x) (and (integer? x) (exact? x) (positive? x))))

;; A persistent-paths entry is (TIER . ABSOLUTE-PATH) with TIER one of
;; 'precious/'disposable and a non-root absolute path — a malformed entry would
;; otherwise emit a bogus bind mount deep in the lowering.
(define (persistent-path-entry? x)
  (and (pair? x)
       (memq (car x) '(precious disposable))
       (absolute-path? (cdr x))
       (not (string=? (cdr x) "/"))))

(define (persistent-paths? x)
  (and (list? x) (every persistent-path-entry? x)))

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

;; The fixed BASE CAPABILITIES the compiler injects into EVERY image regardless
;; of the manifest (M9: crun, the container host; rust-userland 2026-06-17: the
;; Rust-native base userland — procs, fd, ripgrep, sd, eza, bat). These are a
;; mandatory platform invariant — part of "effective = base + payload + markers"
;; — NOT swappable manifest content. The compiler's `packages` field PREPENDS
;; this list (single source of truth) to the payload. That prepend is the
;; by-construction guarantee for the REMOVE half of the contract: every entry is
;; present in every image for ANY manifest, so the manifest cannot REMOVE a base
;; capability. The ADD half is a name-based hygiene PRE-FILTER (the constructor
;; rejects a manifest that names a base capability, directly or via propagation —
;; see below); it is NOT a closure-complete gate, and (unlike guix) needs none.
;; tests/manifest-diff.scm (d) asserts the injection invariant on lowered systems
;; (it checks crun specifically — the canonical capability); tests/typed-coverage.scm
;; asserts the pre-filter rejection (direct + propagated) and the narrowed contract.
;; The frozen oracle (system td) prepends the IDENTICAL list in the same order, so
;; the M4/M5/M6 differentials stay byte-converged.
(define %base-capabilities (list crun procs fd ripgrep sd eza bat))

;; The base capability a manifest redundantly names, or #f — a hygiene PRE-FILTER,
;; not the guarantee. Matched by NAME over the manifest's PROFILE packages (direct
;; entries PLUS transitively-propagated inputs, exactly like `manifest-has-guix?`),
;; so both a same-named crun and a package that PROPAGATES crun are caught. It does
;; NOT catch a RENAMED clone (a package inheriting crun under a different name) — a
;; static name scan provably cannot, just as the guix pre-filter cannot see renamed
;; guix. That gap is acceptable here (no closure gate needed, unlike guix's
;; security contract): the real by-construction guarantee is INJECTION —
;; %base-capabilities is unconditionally prepended in `packages`, so crun is in
;; EVERY image for ANY manifest and the manifest cannot REMOVE it. A renamed crun
;; clone in the payload does not remove or weaken that capability; it is merely
;; redundant payload the contract does not forbid. This pre-filter just turns the
;; common redundant listing (crun by name, direct or propagated) into a fast error.
(define (manifest-base-capability manifest)
  (let ((names (map package-name %base-capabilities)))
    (find (lambda (p) (member (package-name p) names))
          (manifest-profile-packages manifest))))

;;;
;;; The smart constructor. Keyword-driven with defaults that, taken together,
;;; describe EXACTLY the system the hand-written `td-system` declares — so
;;; `%td-default-config` lowers to the oracle's store path.
;;;

;; The single writable filesystem (§2.6) and where the compiler mounts it.
(define %td-state-label "td-state")
(define %td-state-mount-point "/td-state")

;; Where the SSH host key lives — under the model's FIRST allowlist entry, so a
;; rollback never changes machine identity (§2.6 "machine identity ≠ OS
;; identity"). Relocated via sshd_config (HostKey), not by mounting over /etc.
(define %td-ssh-host-key "/var/lib/ssh/ssh_host_ed25519_key")

;; The default allowlist: SSH host keys, precious. (DESIGN §2.6: "SSH host keys
;; are the first entry".)
(define %td-default-persistent-paths
  '((precious . "/var/lib/ssh")))

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
                    (ship-guix? #f)
                    (generation #f)
                    (persistent-paths %td-default-persistent-paths))
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
  (check generation-id? 'generation generation "#f or a positive integer")
  (check persistent-paths? 'persistent-paths persistent-paths
         "a list of (precious|disposable . \"/abs/path\") pairs")
  ;; No two entries may declare the SAME path: they would lower to two bind
  ;; mounts on one mount point (ambiguous tier; colliding per-mount-point fs
  ;; services) — a silently-wrong system, rejected at construction instead.
  (let* ((paths (map cdr persistent-paths))
         (dup   (find (lambda (p)
                        (> (count (lambda (q) (string=? p q)) paths) 1))
                      paths)))
    (when dup
      (error (format #f "td-config: persistent-paths declares ~s more than once — \
one tier per path (two entries would bind-mount the same mount point twice)"
                     dup))))
  ;; Cross-field (§2.6): a GENERATION system relocates its SSH host key under
  ;; /var/lib/ssh — machine identity must live on td-state or a rollback would
  ;; swap it along with the OS. So when a generation id is set, the allowlist
  ;; must keep a PRECIOUS entry covering /var/lib/ssh; dropping it would
  ;; silently put the host key back on the per-generation root.
  (when (and generation
             (not (member '(precious . "/var/lib/ssh") persistent-paths)))
    (error (string-append
            "td-config: a generation system requires the (precious . \"/var/lib/ssh\") "
            "persistent-paths entry — the SSH host key is relocated there so that "
            "machine identity survives a rollback (DESIGN §2.6). Keep that entry "
            "in the allowlist.")))
  ;; Cross-field — the BASE-CAPABILITY boundary (hygiene PRE-FILTER). The manifest
  ;; is the swappable PAYLOAD only; a base capability (crun, the container host) is
  ;; a mandatory platform invariant the compiler injects into every image, not
  ;; manifest content. Naming one (directly or via a propagated input) is a
  ;; category error — and would duplicate it in `packages` — so reject it at
  ;; construction. This is a fast pre-filter for the common mistake, NOT a closure
  ;; gate: a RENAMED clone of crun is not caught (a name scan cannot), and need not
  ;; be — it cannot REMOVE the injected capability, only add redundant payload. The
  ;; by-construction guarantee is the unconditional prepend of %base-capabilities in
  ;; `packages` (the manifest cannot remove crun) — see `manifest-base-capability`.
  ;; (Per DESIGN: we deliberately do NOT expose a user-configurable base-capabilities
  ;; field — the base set is not optional manifest content.)
  (let ((cap (manifest-base-capability manifest)))
    (when cap
      (error (string-append
              "td-config: the manifest must not name a base capability ("
              (package-name cap) ") — base capabilities (e.g. crun, the "
              "container host) are a mandatory platform invariant the compiler "
              "injects into every image, not swappable manifest content. The "
              "manifest drives only the payload; drop it from the manifest."))))
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
                  manifest ship-guix? generation persistent-paths))

;;;
;;; The compiler: typed config -> operating-system (a gexp-bearing value).
;;; This mirrors (system td) field for field. Any drift here shows up as a
;;; store-path divergence in tests/typed-diff.scm — that is the test's job.
;;;

;; The root filesystem label this config boots. For a generation (positive id)
;; it is a DISTINCT, bootloader-selectable label `<root>-gen-<n>`, so each
;; generation mounts its own root and rollback is real (M10.1, M10-design.md
;; P1). With no generation (#f) it is the plain `root-fs-label`, so the default
;; config stays byte-identical to the frozen oracle's shared `td-root`.
(define (td-config-effective-root-label c)
  (let ((base (td-config-root-fs-label c))
        (gen  (td-config-generation c)))
    (if gen
        (format #f "~a-gen-~a" base gen)
        base)))

;; The §2.6 state model, compiled (generation mode only). One writable
;; filesystem — label td-state, mounted needed-for-boot (the initrd mounts it,
;; so the activation below can already write through its BACKING paths before
;; shepherd starts anything) — plus one bind mount per allowlist entry, mounted
;; by shepherd's file-system services (which `user-processes`, and so every
;; daemon, requires — sshd never starts before its key directory is bound).

(define %td-state-file-system
  (file-system
    (device (file-system-label %td-state-label))
    (mount-point %td-state-mount-point)
    (type "ext4")
    (needed-for-boot? #t)))

(define (persistent-path-tier-directory tier)
  (case tier
    ((precious)   "state")
    ((disposable) "cache")))

;; The backing directory on td-state for an allowlist entry — the tier root
;; plus the entry's own path (state/var/lib/ssh, cache/var/log, ...).
(define (persistent-path-backing entry)
  (match entry
    ((tier . path)
     (string-append %td-state-mount-point "/"
                    (persistent-path-tier-directory tier) path))))

;; No `dependencies` on td-state here: a needed-for-boot filesystem has no
;; shepherd service to depend on — and none is needed, since the initrd mounts
;; td-state before shepherd (and these binds) exist at all.
(define (persistent-path-file-system entry)
  (file-system
    (device (persistent-path-backing entry))
    (mount-point (cdr entry))
    (type "none")
    (flags '(bind-mount))))

;; Activation: create the tier roots + each entry's backing dir and mount
;; point (idempotent), then generate the machine's SSH host key — THROUGH THE
;; BACKING PATH, because activation runs after the initrd mounted td-state but
;; before shepherd mounts the binds. First boot mints the identity onto
;; td-state; every later boot (and every rollback) finds it there and leaves it
;; alone.
(define (td-state-activation entries)
  (with-imported-modules '((guix build utils))
    #~(begin
        (use-modules (guix build utils))
        (mkdir-p (string-append #$%td-state-mount-point "/state"))
        (mkdir-p (string-append #$%td-state-mount-point "/cache"))
        (mkdir-p (string-append #$%td-state-mount-point "/home"))
        (for-each (lambda (backing+mount)
                    (mkdir-p (car backing+mount))
                    (mkdir-p (cdr backing+mount)))
                  '#$(map (lambda (e)
                            (cons (persistent-path-backing e) (cdr e)))
                          entries))
        (let ((key (string-append #$%td-state-mount-point "/state"
                                  #$%td-ssh-host-key)))
          (unless (file-exists? key)
            (invoke #$(file-append openssh "/bin/ssh-keygen")
                    "-q" "-t" "ed25519" "-N" "" "-f" key))))))

(define (td-config->operating-system c)
  ;; M11 (§2.6 enforcement stage): the ONE mapped device a generation system
  ;; boots through — its root partition (slot-bound by label) opened as the
  ;; dm-verity target, hash-checked on every read.
  (define gen (td-config-generation c))
  (define verity-md
    (and gen (td-verity-mapped-device (td-config-effective-root-label c))))

  (operating-system
    (host-name (td-config-host-name c))
    (timezone (td-config-timezone c))
    (locale (td-config-locale c))

    (bootloader
     (bootloader-configuration
      (bootloader grub-bootloader)
      (targets (list (td-config-bootloader-target c)))))

    ;; M11: generation mode boots through the verity device (it must be in
    ;; this field to reach the initrd — operating-system-boot-mapped-devices
    ;; collects it via the store file-system's dependencies and its
    ;; /dev/mapper/td-root device string) and needs the
    ;; dm-verity module loaded before the initrd's pre-mount opens it. With
    ;; generation #f both stay the operating-system defaults, byte-identical
    ;; to the frozen oracle.
    (mapped-devices (if gen (list verity-md) '()))
    (initrd-modules (if gen
                        (cons "dm-verity" %base-initrd-modules)
                        %base-initrd-modules))

    ;; Root fs + the cgroup2 container-host mount (M9), shared with the oracle.
    ;; M10.3 (§2.6): a GENERATION system additionally mounts the single
    ;; writable filesystem (td-state, needed-for-boot) and bind-mounts each
    ;; declared persistent path from its tier directory there — default-deny
    ;; persistence: nothing else survives a generation swap. With generation #f
    ;; none of this is emitted, so the default config still lowers
    ;; byte-identically to the frozen oracle.
    ;; M11 (§2.6 "the root is assembled, not stored"): a generation's "/" is
    ;; now a TMPFS the boot path may write — activation materializes /etc,
    ;; /run, /tmp on it, and nothing on it survives a reboot. The OS content
    ;; comes from the generation image, opened through dm-verity and mounted
    ;; READ-ONLY at /gnu/store: an undeclared write into it fails closed
    ;; (EROFS) and a corrupted block read fails closed (EIO) — read-only by
    ;; kernel enforcement now, not convention. No fsck on either: tmpfs has
    ;; none, and for the store dm-verity IS the integrity check (fsck could
    ;; not write the sealed device anyway). The store image is ext4 by the
    ;; placer's mkfs contract (a read-only container format — §2.6),
    ;; independent of the config's root-fs-type.
    (file-systems
     (append
      (cons* (if gen
                 (file-system
                   (device "none")
                   (mount-point (td-config-root-mount c))
                   (type "tmpfs")
                   (check? #f)
                   (needed-for-boot? #t))
                 (file-system
                   (device (file-system-label (td-config-effective-root-label c)))
                   (mount-point (td-config-root-mount c))
                   (type (td-config-root-fs-type c))))
             cgroup2-file-system
             (if gen
                 (cons* (file-system
                          (device (string-append "/dev/mapper/"
                                                 %td-verity-target))
                          (mount-point "/gnu/store")
                          (type "ext4")
                          (flags '(read-only))
                          (check? #f)
                          (needed-for-boot? #t)
                          (dependencies (list verity-md)))
                        %td-state-file-system
                        (map persistent-path-file-system
                             (td-config-persistent-paths c)))
                 '()))
      %base-file-systems))

    ;; The image's EFFECTIVE package set is layered (M6, made precise by triage
    ;; F-review #2): effective = fixed base capabilities (crun, below) + the
    ;; manifest-selected payload + enforcement markers (guix-free-marker, below).
    ;; The manifest drives ONLY the swappable payload — it is NOT the whole
    ;; package set, and cannot remove or control the injected canonical base
    ;; capability (a renamed crun clone in the payload is permitted, but it
    ;; cannot remove the canonical crun the compiler injects). The default
    ;; manifest is %base-packages, which is exactly the operating-system field's
    ;; own default. A non-default manifest is a different image: a whole-image
    ;; swap, not an in-place install. (tests/manifest-diff.scm (c) pins the
    ;; payload to the manifest; (d) pins crun OUTSIDE it.)
    ;;
    ;; M7 (F1, embedded gate): for a hardened (#f) config we ALSO prepend the
    ;; `guix-free-marker` — a build-time package whose build FAILS if guix is
    ;; anywhere in the (other) packages' closure. Because it lives in `packages`,
    ;; EVERY lowering of this system (bare operating-system, qcow2, docker, any
    ;; helper) builds the profile and therefore the marker, so a hardened image is
    ;; guix-free OR it does not build — by construction, with no bypassable opt-in.
    ;; Since #f is now the SHIPPED default (signed off §4.3), the re-baselined frozen
    ;; oracle (system td) embeds this same marker, so the default config still lowers
    ;; byte-for-byte to it (§2.5) — at the new guix-free digest. An explicit #t
    ;; config takes the manifest verbatim (no marker) and diverges.
    ;; M9: the base capabilities (`crun`) are shipped regardless of the user
    ;; manifest — they are container-host capabilities, not swappable manifest
    ;; entries — so they are prepended here (outside `manifest`) from the single
    ;; %base-capabilities source the constructor also guards. With %base-capabilities
    ;; = (list crun), this is exactly the oracle's (cons crun %base-packages). This
    ;; prepend is the by-construction guarantee crun is always present (the manifest
    ;; cannot remove it); the constructor's name pre-filter rejects a manifest that
    ;; redundantly names a base capability (direct or propagated) so the common
    ;; duplication is caught early. The guix-free-marker scans this set (crun pulls
    ;; in no guix).
    (packages (let ((pkgs (append %base-capabilities (td-config-manifest c))))
                (if (td-config-ship-guix? c)
                    pkgs
                    (cons (guix-free-marker pkgs) pkgs))))

    ;; M7: when `ship-guix?` is #f (now the shipped default), delete
    ;; `guix-service-type` so the realized image carries no `guix`/`guix-daemon`
    ;; binary — image-swap-only by construction (no in-image `guix install`) — AND
    ;; add `guix-free-privsep-service`, which restores the sshd privsep dir
    ;; (/var/empty) guix-service-type used to set up as a side effect (without it a
    ;; guix-free sshd aborts every connection). The re-baselined oracle (system td)
    ;; does both the same way, so the default config stays byte-identical (the
    ;; M4/M5/M6 differentials keep converging). An explicit #t config keeps
    ;; guix-service-type (which provides /var/empty) and omits the privsep fix, so
    ;; it diverges.
    ;; M10.3 (§2.6): a GENERATION system relocates the SSH host key onto the
    ;; precious tier (HostKey under /var/lib/ssh — bind-mounted from
    ;; td-state/state) so a rollback swaps the OS but never the machine
    ;; identity, and adds the activation that creates the backing dirs and
    ;; mints the key on first boot. generate-host-keys? is switched off there —
    ;; the relocated key IS the host key; /etc/ssh keys would be per-generation
    ;; noise. All of it generation-gated: the default config's services stay
    ;; byte-identical to the oracle's.
    (services
     (let* ((gen  (td-config-generation c))
            (dhcp (service dhcpcd-service-type))
            (ssh  (service openssh-service-type
                           (openssh-configuration
                            (port-number (td-config-ssh-port c))
                            (password-authentication?
                             (td-config-ssh-password-auth? c))
                            (challenge-response-authentication?
                             (td-config-ssh-challenge-response? c))
                            (generate-host-keys? (not gen))
                            (extra-content
                             (if gen
                                 (string-append "HostKey " %td-ssh-host-key)
                                 "")))))
            (state (if gen
                       (list (simple-service 'td-state-activation
                                             activation-service-type
                                             (td-state-activation
                                              (td-config-persistent-paths c))))
                       '())))
       (if (td-config-ship-guix? c)
           (append (cons* dhcp ssh state) %base-services)
           (modify-services
               (append (cons* dhcp ssh guix-free-privsep-service state)
                       %base-services)
             (delete guix-service-type)))))))

;; The default typed config — by construction equal in content to `td-system`.
(define %td-default-config (td-config))
