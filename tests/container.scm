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
;; M8's sandbox-only concern). The app's own `hello` binary is the entrypoint, run
;; from the image's own closure (its glibc loader, also in the image).
;;
;; Self-discriminating (the M3 lesson): a POSITIVE run must print the app's output
;; ("Hello, world!") and exit 0; a NEGATIVE control (the same rootfs, but the
;; config execs a bogus path) must FAIL. The positive proves bundle setup is sound,
;; so the negative isolates "did the app binary actually run".
(define-module (tests container)
  #:use-module (gnu tests)
  #:use-module (gnu system)
  #:use-module (gnu system vm)
  #:use-module (gnu packages base)         ;hello, tar
  #:use-module (gnu packages compression)  ;gzip
  #:use-module (guix gexp)
  #:use-module (guix monads)               ;mlet, %store-monad
  #:use-module (guix store)
  #:use-module (guix profiles)             ;profile, packages->manifest
  #:use-module (guix scripts pack)         ;docker-image
  #:use-module (system td)
  #:export (%test-td-container))

;; A minimal OCI APP image: a profile with just GNU hello, packed as a Docker
;; image whose entry-point is the hello binary. This is `guix pack -f docker
;; --entry-point=bin/hello hello`, expressed in Scheme.
(define app-profile
  (profile (content (packages->manifest (list hello)))))

(define (app-image-mvalue)
  (docker-image "td-app-hello" app-profile
                #:entry-point "bin/hello"
                #:localstatedir? #f))

;; Unpack the app image into a runtime-bundle ROOTFS (the OCI runtime spec's
;; root.path) at build time. The output IS the rootfs: the image's layer extracted,
;; plus the /proc and /dev mountpoints crun needs. The image carries hello's full
;; closure (incl. its glibc loader), so the rootfs is self-contained. mlet binds the
;; (monadic) docker-image to a derivation so #$image references it.
(define (app-rootfs-mvalue)
  (mlet %store-monad ((image (app-image-mvalue)))
    (gexp->derivation "td-app-hello-rootfs"
      (with-imported-modules '((guix build utils))
        #~(begin
            (use-modules (guix build utils))
            (define rootfs #$output)
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
            (mkdir-p (string-append rootfs "/dev")))))))

(define (run-container-test)
  (mlet %store-monad ((rootfs (app-rootfs-mvalue)))
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
                           (ice-9 rdelim))

              (define marionette (make-marionette (list #$vm)))
              (test-runner-current (system-test-runner #$output))
              (test-begin "td-container")

              ;; Build-side values (lowered to strings) used inside the guest.
              ;; root.path points straight at the read-only store rootfs — crun runs
              ;; it directly (verified), so there is no copy and thus no guest-tmpfs
              ;; pressure (copying hello's ~70MB closure overflowed /tmp).
              (define rootfs-path #$rootfs)
              (define crun-bin "/run/current-system/profile/bin/crun")
              ;; An OCI runtime config.json execing EXE — same shape the M9 gate
              ;; proved: real root (no userns), cgroups disabled, empty network ns,
              ;; /proc + tmpfs /dev. Read-only store rootfs.
              (define (config-json exe)
                (string-append
                 "{\"ociVersion\":\"1.0.0\",\"process\":{\"terminal\":false,"
                 "\"user\":{\"uid\":0,\"gid\":0},\"args\":[\"" exe "\"],"
                 "\"env\":[\"PATH=/bin:/usr/bin\",\"HOME=/\",\"TERM=dumb\"],"
                 "\"cwd\":\"/\"},\"root\":{\"path\":\"" rootfs-path "\",\"readonly\":true},"
                 "\"hostname\":\"td-app\",\"mounts\":[{\"destination\":\"/proc\","
                 "\"type\":\"proc\",\"source\":\"proc\"},{\"destination\":\"/dev\","
                 "\"type\":\"tmpfs\",\"source\":\"tmpfs\",\"options\":[\"nosuid\","
                 "\"strictatime\",\"mode=755\",\"size=65536k\"]}],\"linux\":{"
                 "\"namespaces\":[{\"type\":\"pid\"},{\"type\":\"mount\"},"
                 "{\"type\":\"ipc\"},{\"type\":\"uts\"},{\"type\":\"network\"}]}}"))
              (define good-config (config-json #$(file-append hello "/bin/hello")))
              (define bad-config  (config-json "/bin/td-nonexistent-app-xyz"))

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

              ;; POSITIVE: the app image runs on the booted base and prints its
              ;; output. Proves the SHIPPED crun (from /run/current-system) runs a
              ;; real OCI app container to completion (claim d: crun is the base's).
              (let ((pos (run-app good-config "td-app-pos")))
                (format #t "POS: ~s~%" pos)
                (test-assert "shipped base runs an OCI app container (crun, as root)"
                  (and (eqv? 0 (car pos))
                       (string-contains (cdr pos) "Hello, world!"))))

              ;; NEGATIVE control: same bundle, bogus entrypoint -> must FAIL, so a
              ;; green positive means the app really ran (not a vacuous pass).
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
(guix pack -f docker hello) on it with the shipped crun, as root: assert the app \
prints its output and exits 0, with a negative control proving the rung \
discriminates.")
   (value (run-container-test))))
