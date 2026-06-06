;; system/td-hardening.scm — M7 triage F1 (round 3): the REAL guix-free guarantee.
;;
;; Earlier rounds tried to make `ship-guix? #f` honest with a STATIC check in the
;; `td-config` constructor (reject a manifest naming `guix`, then also one
;; transitively PROPAGATING it). External review round 3 showed that family of
;; checks is fundamentally incomplete: a package can land `bin/guix` in the image
;; through paths a name/propagation scan cannot see —
;;   * a plain RUNTIME REFERENCE to guix (guix in `inputs`/a retained store-path
;;     string in the output) — not a propagated input, so the profile scan misses it;
;;   * a RENAMED package inheriting guix (`(package (inherit guix) (name "x"))`) —
;;     its name is not "guix", so the name scan misses it.
;; Both put a real `.../bin/guix` in the realized image's store closure.
;;
;; The only honest, manifest-AGNOSTIC guarantee is therefore at the CLOSURE level,
;; enforced at BUILD time: scan the realized artifact and FAIL the build if any
;; `bin/guix`/`bin/guix-daemon` is present. `assert-guix-free-image` is that gate;
;; `guix-free-docker-image` wires it as a build dependency of the image, so the
;; SUPPORTED way to build a hardened OCI image (`td-config->guix-free-docker-image`)
;; is guix-free *or it does not build* — for ANY manifest, not just a fixture.
;;
;; The constructor's name/propagation check (see (system td-typed)) is retained
;; only as a CHEAP PRE-FILTER that fails the obvious `(list guix)` mistake in
;; sub-second time before an expensive build; it is explicitly NOT the guarantee.
;; This gate is.
(define-module (system td-hardening)
  #:use-module (guix gexp)
  #:use-module (gnu system image)        ;system-image, image-with-os, docker-image
  #:use-module (gnu packages base)       ;tar, coreutils, grep
  #:use-module (gnu packages bash)       ;bash
  #:use-module (gnu packages compression) ;gzip
  #:use-module (system td-typed)
  #:export (assert-guix-free-image
            guix-free-docker-image
            td-config->guix-free-docker-image))

(define* (assert-guix-free-image image #:key (name "td-guix-free-gate"))
  "Return a <computed-file> that builds (an empty output) IFF the docker .tar.gz
file-like IMAGE packs no `bin/guix` or `bin/guix-daemon`; otherwise the build
FAILS. Because it inspects the realized artifact's packed store closure, it catches
guix arriving via ANY path — runtime reference, renamed/inherited package, or
propagation — which the constructor's name-based profile pre-filter cannot. This is
the closure-level, manifest-agnostic guix-free guarantee for `ship-guix? #f`."
  (computed-file name
    (with-imported-modules '((guix build utils))
      #~(begin
          (use-modules (guix build utils))
          (setenv "PATH"
                  (string-join (list #$(file-append tar "/bin")
                                     #$(file-append gzip "/bin")
                                     #$(file-append grep "/bin")
                                     #$(file-append coreutils "/bin"))
                               ":"))
          ;; Same probe as the no-guix Makefile rung, but INSIDE the build graph so
          ;; it gates the artifact: list every path packed in the image's
          ;; layer.tar(s) and count `.../bin/guix(-daemon)` entries. We use `grep
          ;; -Ec` (count), NOT `grep -Eq`: -q exits early on the first match, which
          ;; under `set -o pipefail` makes the upstream `tar` die of SIGPIPE (141)
          ;; and corrupts the pipeline status precisely in the guix-FOUND case. -c
          ;; reads all input, so the exit status is honest: 0 == FOUND (≥1 match,
          ;; gate must fail), 1 == none (ok), and pipefail surfaces any upstream tar
          ;; read error as a non-zero/non-{0,1} status (fail closed: refuse to
          ;; certify an unreadable archive guix-free).
          (let ((status
                 (status:exit-val
                  (system* #$(file-append bash "/bin/bash") "-c"
                           (string-append
                            "set -o pipefail; "
                            "tar xzOf " #$image " --wildcards '*/layer.tar' "
                            "| tar tf - | grep -Ec '/bin/guix(-daemon)?$'")))))
            (cond
             ((eqv? status 0)
              (error "td hardening: ship-guix? #f image STILL contains a \
guix/guix-daemon binary in its closure — the imperative surface was not removed."))
             ((eqv? status 1)
              (mkdir #$output))
             (else
              (error "td hardening: could not scan the image archive (corrupt or \
unreadable); refusing to certify it guix-free." status))))))))

(define* (guix-free-docker-image os #:key (name "td-guix-free-docker-image"))
  "Return a file-like that builds to OS's docker image, but ONLY if that image is
guix-free: `assert-guix-free-image` is a build dependency, so a guix-ful hardened
image FAILS to build rather than silently shipping the imperative surface. Use this
— not the bare `system-image` — to build a hardened (ship-guix? #f) OCI image."
  (let ((image (system-image (image-with-os docker-image os))))
    (computed-file name
      #~(begin
          ;; Force the gate: producing this output requires building the gate,
          ;; which FAILS the build if the image is not guix-free.
          (let ((_ #$(assert-guix-free-image image))) #t)
          ;; The certified output IS the image (a symlink keeps it one store item).
          (symlink #$image #$output)))))

(define* (td-config->guix-free-docker-image c #:key (name "td-guix-free-docker-image"))
  "The SUPPORTED way to build a hardened OCI image from a typed config: lower C to
an operating-system and return its build-gated, guix-free-or-fails docker image."
  (guix-free-docker-image (td-config->operating-system c) #:name name))
