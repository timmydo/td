;; tests/container.scm — M9.2: the booted td base is a working OCI container HOST.
;;
;; M9.1 made the base a container host by construction (it ships crun and mounts
;; cgroup2 — asserted by tests/boot.scm). This rung proves the capability end to
;; end: boot the shipped base and RUN a real OCI APP image on it with the SHIPPED
;; crun, as root, to completion.
;;
;; Contrast with M8's `run` rung: M8 ran the shipped SYSTEM image's own userspace
;; (one declaration → VM + OCI). M9 runs a SEPARATE app image ON the booted base —
;; the container-HOST relationship, the runtime OCI *app model* (DESIGN §2.3).
;;
;; The app is a Guix-built OCI image (`guix pack -f docker` of GNU hello): a store
;; path, so it is in the guest's closure with NO registry pull — the loop stays
;; offline (the container also runs with an empty network namespace). We unpack the
;; image into a runtime-bundle rootfs at BUILD time (hermetic, full tooling), then
;; in the guest crun runs it AS ROOT — no rootless/userns contortions (that was
;; M8's sandbox-only concern). The app runs via the IMAGE'S OWN declared entrypoint
;; (see below), from the image's own closure (its glibc loader, also in the image).
;;
;; Honor the declared entrypoint (triage F1, the false-green fix): the OCI process
;; args are NOT a host-known store path — they are read out of the app image's own
;; archive (manifest.json -> Config -> .config.entrypoint) at BUILD time and emitted
;; alongside the rootfs as args.json. The guest execs exactly those args. So the
;; IMAGE'S metadata drives the run: change `#:entry-point` to a bogus value and the
;; positive FAILS (the earlier version hardcoded hello/bin/hello and could not tell).
;; The entrypoint is read with a STRUCTURED JSON parser (guix build json), not a
;; brittle regex.
;;
;; Self-discriminating (the M3 lesson): a POSITIVE run must print the app's output
;; ("Hello, world!") and exit 0; TWO negative controls must FAIL:
;;   - IMAGE-METADATA negative (F-review): a SECOND app image whose DECLARED
;;     entrypoint is bogus (td-app-badentry-image) — its bundle's args.json carries
;;     that bad path, so the run fails. This proves the run is driven by the image's
;;     own metadata, not merely a runtime arg the test happens to supply.
;;   - RUNTIME-ARG negative: the good rootfs run with a bogus arg path also fails.
;; The positive proves bundle setup is sound, so the negatives isolate "did the
;; app's DECLARED entrypoint actually run".
(define-module (tests container)
  #:use-module (gnu tests)
  #:use-module (gnu system)
  #:use-module (gnu system vm)
  #:use-module (gnu packages base)         ;hello, tar
  #:use-module (gnu packages compression)  ;gzip
  #:use-module (gnu packages guile)        ;guile-json-4 — structured JSON parse
  #:use-module (guix gexp)
  #:use-module (guix monads)               ;mlet, %store-monad
  #:use-module (guix store)
  #:use-module (guix profiles)             ;profile, packages->manifest
  #:use-module (guix scripts pack)         ;docker-image
  #:use-module (system td)
  #:export (td-app-image
            td-app-bundle
            td-app-badentry-image
            td-app-badentry-bundle
            %test-td-container))

;; A minimal OCI APP image: a profile with just GNU hello, packed as a Docker
;; image whose entry-point is the hello binary. This is `guix pack -f docker
;; --entry-point=bin/hello hello`, expressed in Scheme.
(define app-profile
  (profile (content (packages->manifest (list hello)))))

;; An app image over the SAME profile but with the given declared entry-point.
;; Parameterizing the entry-point lets us build a second image whose declared
;; entrypoint is bogus (below) — the negative that proves the run honors the
;; IMAGE'S metadata, not a host-known path.
(define (app-image name entry)
  (docker-image name app-profile #:entry-point entry #:localstatedir? #f))

;; Public (the `container` rung --check's this artifact's reproducibility).
(define (td-app-image)
  (app-image "td-app-hello" "bin/hello"))

;; A second app image, identical profile, but whose DECLARED entrypoint points
;; at a path that does not exist in the profile. The bundle below reads THIS
;; image's own entrypoint into its args.json, so running it must fail — proving
;; the run is driven by image metadata, not by a runtime arg override.
(define (td-app-badentry-image)
  (app-image "td-app-badentry" "bin/td-nonexistent-app-xyz"))

;; Unpack an app image into a runtime BUNDLE at build time. The output is a
;; directory holding:
;;   rootfs/    — the image's layer extracted, plus /proc and /dev mountpoints.
;;                The image carries hello's full closure (incl. its glibc loader),
;;                so the rootfs is self-contained.
;;   args.json  — the image's DECLARED entrypoint, read from the archive's config
;;                (manifest.json -> Config -> .config.entrypoint), as a JSON array.
;;                The guest execs exactly these args, so a bogus declared entry
;;                point yields a failing run (td-app-badentry-* below).
;; The entrypoint is read with a STRUCTURED JSON parser (guile-json's (json),
;; added to the build via with-extensions), not a regex: json->scm renders a JSON
;; object as a (key . value) alist and an array as a vector, so we navigate
;; manifest[0].Config -> config.config.entrypoint and re-emit it with scm->json.
;; mlet binds the (monadic) docker-image to a derivation so #$image references it.
(define (app-bundle name image)
  (mlet %store-monad ((image image))
    (gexp->derivation name
      (with-extensions (list guile-json-4)
        (with-imported-modules '((guix build utils))
          #~(begin
              (use-modules (guix build utils)
                           (json))
              (define out #$output)
              (define rootfs (string-append out "/rootfs"))
              (mkdir-p rootfs)
              (mkdir-p "extract")
              ;; The docker archive is a gzip'd tar; extract it, then extract its
              ;; single layer.tar into the rootfs. Absolute tar/gzip — the build
              ;; sandbox has no PATH to rely on.
              (invoke #$(file-append tar "/bin/tar")
                      "--use-compress-program" #$(file-append gzip "/bin/gzip")
                      "-xf" #$image "-C" "extract")
              (let ((layer (car (find-files "extract" "layer\\.tar$"))))
                (invoke #$(file-append tar "/bin/tar") "-xf" layer "-C" rootfs))
              (mkdir-p (string-append rootfs "/proc"))
              (mkdir-p (string-append rootfs "/dev"))
              ;; Read the image's declared entrypoint out of its own config and
              ;; emit it as the OCI process args. manifest.json is a one-element
              ;; array (vector) of objects; manifest[0].Config names the config
              ;; file, whose config.entrypoint is a JSON array (vector) of absolute
              ;; paths into the image's profile (the layer resolves them via its
              ;; bin symlink).
              (define manifest
                (call-with-input-file "extract/manifest.json" json->scm))
              (define config-name (assoc-ref (vector-ref manifest 0) "Config"))
              (define config
                (call-with-input-file (string-append "extract/" config-name)
                  json->scm))
              (define entrypoint
                (assoc-ref (assoc-ref config "config") "entrypoint"))
              (unless (and (vector? entrypoint) (> (vector-length entrypoint) 0))
                (error "no entrypoint in app image config" config-name))
              (call-with-output-file (string-append out "/args.json")
                (lambda (p) (scm->json entrypoint p)))))))))

;; Public (the `container` rung --check's this artifact's reproducibility, and
;; transitively the image's, since the bundle depends on it).
(define (td-app-bundle)
  (app-bundle "td-app-hello-bundle" (td-app-image)))

;; The bad-entrypoint bundle: same unpack path, but args.json carries the bogus
;; declared entrypoint — running it must fail (the image-metadata negative).
(define (td-app-badentry-bundle)
  (app-bundle "td-app-badentry-bundle" (td-app-badentry-image)))

(define (run-container-test)
  (mlet %store-monad ((bundle          (td-app-bundle))
                      (badentry-bundle (td-app-badentry-bundle)))
    (let* ((os (marionette-operating-system
                td-system
                #:imported-modules '((gnu services herd))))
           (vm (virtual-machine os)))
      (gexp->derivation "td-container-test"
        (with-imported-modules '((gnu build marionette))
          #~(begin
              (use-modules (gnu build marionette)
                           (srfi srfi-64)
                           (srfi srfi-13)
                           (ice-9 popen)
                           (ice-9 rdelim)
                           (ice-9 textual-ports))

              (define marionette (make-marionette (list #$vm)))
              (test-runner-current (system-test-runner #$output))
              (test-begin "td-container")

              ;; Build-side values (lowered to strings) used inside the guest.
              ;; root.path points straight at the read-only store rootfs — crun runs
              ;; it directly (verified), so there is no copy and thus no guest-tmpfs
              ;; pressure (copying hello's ~70MB closure overflowed /tmp).
              (define bundle-path #$bundle)
              (define rootfs-path (string-append bundle-path "/rootfs"))
              (define badentry-bundle-path #$badentry-bundle)
              (define badentry-rootfs-path
                (string-append badentry-bundle-path "/rootfs"))
              (define crun-bin "/run/current-system/profile/bin/crun")
              ;; Each app image's DECLARED entrypoint, read from its own bundle
              ;; (extracted from the image archive at build time). This is what
              ;; makes the run honor the image rather than a host-known path.
              (define (read-args bundle)
                (call-with-input-file (string-append bundle "/args.json")
                  get-string-all))
              (define image-args    (read-args bundle-path))
              (define badentry-args (read-args badentry-bundle-path))
              ;; An OCI runtime config.json over ROOTFS whose process.args is the
              ;; JSON array ARGS-JSON — same shape the M9 gate proved: real root
              ;; (no userns), cgroups disabled, empty network ns, /proc + tmpfs
              ;; /dev. Read-only store rootfs.
              (define (config-json rootfs args-json)
                (string-append
                 "{\"ociVersion\":\"1.0.0\",\"process\":{\"terminal\":false,"
                 "\"user\":{\"uid\":0,\"gid\":0},\"args\":" args-json ","
                 "\"env\":[\"PATH=/bin:/usr/bin\",\"HOME=/\",\"TERM=dumb\"],"
                 "\"cwd\":\"/\"},\"root\":{\"path\":\"" rootfs "\",\"readonly\":true},"
                 "\"hostname\":\"td-app\",\"mounts\":[{\"destination\":\"/proc\","
                 "\"type\":\"proc\",\"source\":\"proc\"},{\"destination\":\"/dev\","
                 "\"type\":\"tmpfs\",\"source\":\"tmpfs\",\"options\":[\"nosuid\","
                 "\"strictatime\",\"mode=755\",\"size=65536k\"]}],\"linux\":{"
                 "\"namespaces\":[{\"type\":\"pid\"},{\"type\":\"mount\"},"
                 "{\"type\":\"ipc\"},{\"type\":\"uts\"},{\"type\":\"network\"}]}}"))
              ;; POSITIVE execs the image's own declared entrypoint. The
              ;; IMAGE-METADATA negative execs the SECOND image's own (bogus)
              ;; declared entrypoint, from its own rootfs. The RUNTIME-ARG negative
              ;; execs a bogus path against the good rootfs.
              (define good-config     (config-json rootfs-path image-args))
              (define badentry-config (config-json badentry-rootfs-path badentry-args))
              (define bad-config      (config-json rootfs-path
                                                   "[\"/bin/td-nonexistent-app-xyz\"]"))

              ;; The bundle dir holds only config.json (root.path is the absolute
              ;; store rootfs); crun's --root state lives here too.
              (marionette-eval '(mkdir "/tmp/app") marionette)

              ;; Run a container with the given config under container-id TAG;
              ;; return (exit-status . output).
              (define (run-app config tag)
                (marionette-eval
                 `(begin
                    (use-modules (ice-9 popen) (ice-9 rdelim))
                    (call-with-output-file "/tmp/app/config.json"
                      (lambda (p) (display ,config p)))
                    (let* ((cmd (string-append
                                 "cd /tmp/app && " ,crun-bin
                                 " --cgroup-manager=disabled run " ,tag " 2>&1"))
                           (port (open-input-pipe cmd))
                           (out (read-string port))
                           (st (close-pipe port)))
                      (cons (status:exit-val st) out)))
                 marionette))

              ;; POSITIVE: the app image runs on the booted base, via its OWN
              ;; declared entrypoint, and prints its output. Proves the SHIPPED crun
              ;; (from /run/current-system) runs a real OCI app container to
              ;; completion (claim d: crun is the base's).
              (let ((pos (run-app good-config "td-app-pos")))
                (format #t "POS args=~s -> ~s~%" image-args pos)
                (test-assert "shipped base runs an OCI app container (crun, as root)"
                  (and (eqv? 0 (car pos))
                       (string-contains (cdr pos) "Hello, world!"))))

              ;; IMAGE-METADATA negative: a SECOND app image whose DECLARED
              ;; entrypoint is bogus. Its bundle's args.json carries that bad path
              ;; (read the same honest way as the positive), so the run must FAIL.
              ;; This proves the run is driven by the image's own metadata: a
              ;; different image yields a different outcome, with no host override.
              (let ((badentry (run-app badentry-config "td-app-badentry")))
                (format #t "BADENTRY args=~s -> ~s~%" badentry-args badentry)
                (test-assert "an app image with a bogus DECLARED entrypoint fails"
                  (not (eqv? 0 (car badentry)))))

              ;; RUNTIME-ARG negative control: same good bundle, bogus runtime arg
              ;; -> must FAIL, so a green positive means the app really ran (not a
              ;; vacuous pass).
              (let ((neg (run-app bad-config "td-app-neg")))
                (format #t "NEG: ~s~%" neg)
                (test-assert "a bogus app entrypoint fails (rung discriminates)"
                  (not (eqv? 0 (car neg)))))

              (test-end)
              (exit (zero? (test-runner-fail-count (test-runner-current))))))))))

(define %test-td-container
  (system-test
   (name "td-container")
   (description "Boot the shipped td base and run a Guix-built OCI app image \
(guix pack -f docker hello) on it with the shipped crun, as root, via the image's \
OWN declared entrypoint: assert the app prints its output and exits 0. Two \
negative controls prove the rung discriminates: a SECOND image whose DECLARED \
entrypoint is bogus must fail (image metadata drives the run), and a bogus \
runtime arg must fail.")
   (value (run-container-test))))
