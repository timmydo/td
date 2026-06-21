;; tests/build-hermetic-drv.scm — emit a PROBE derivation for the build-hermetic
;; gate (mk/gates/356, own-builder-daemon increments 5 + 6). The probe's builder
;; FAILS the build unless td's build sandbox is isolated from the loop container in
;; two ways a hermetic build requires:
;;   (a) FILESYSTEM: no host path the outer loop-sandbox exposes is reachable —
;;       /var/guix (the guix daemon db/socket/gc-roots, bound rw into the loop)
;;       must be ABSENT. Holds only because sandbox::build pivot_roots into a
;;       minimal root that drops the invoking filesystem.
;;   (b) PID NAMESPACE: the build runs in its OWN pid namespace, so the launching
;;       `td-builder' process (and the rest of the loop's process tree) is INVISIBLE
;;       in /proc. Holds only because sandbox::build unshares NEWPID, forks the
;;       builder to PID 1, and mounts a fresh procfs.
;; So `td-builder realize` of this drv succeeds ONLY because of both.
;;
;; The daemon builds it here to realize the guile closure inputs and prove the probe
;; is well-formed; the daemon's own build chroot also has no /var/guix and pid-
;; namespaces the build (no td-builder there either), so it passes too — the
;; discriminating environment is td's sandbox, exercised by the gate's realize.
;;
;; Emits: PROBE_DRV (the .drv file name), PROBE_OUT (its output path).
(use-modules (guix) (guix gexp) (guix monads) (guix derivations))

(with-store store
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (let ((drv (run-with-store store
               (gexp->derivation "td-build-hermetic-probe"
                 #~(begin
                     (use-modules (ice-9 ftw) (ice-9 rdelim) (srfi srfi-1))
                     ;; (a) Filesystem hermeticity: a hermetic build must not reach
                     ;; the guix daemon state. Holds ONLY because sandbox::build
                     ;; pivot_roots into a minimal root.
                     (when (file-exists? "/var/guix")
                       (error "LEAK: /var/guix reachable inside td's build sandbox"))
                     ;; Sanity: the staged store IS present (the build is not empty).
                     (unless (file-exists? "/gnu/store")
                       (error "no /gnu/store in td's build sandbox"))
                     ;; (b) Pid-namespace isolation: the build runs in its OWN pid
                     ;; namespace, so the launching `td-builder' process (and the
                     ;; rest of the loop's process tree) is INVISIBLE — only the
                     ;; build's own processes appear in /proc. Holds ONLY because
                     ;; sandbox::build unshares NEWPID, forks the builder to PID 1,
                     ;; and mounts a fresh procfs for that namespace. (Note: under
                     ;; guix-daemon's own pid-namespaced build chroot this also
                     ;; holds — no td-builder there either — so the discriminating
                     ;; environment is td's realize, exercised by the gate.)
                     (let* ((pids (filter (lambda (e)
                                            (and (positive? (string-length e))
                                                 (every char-numeric? (string->list e))))
                                          (or (scandir "/proc") '())))
                            (comm (lambda (pid)
                                    (catch #t
                                      (lambda ()
                                        (call-with-input-file
                                            (string-append "/proc/" pid "/comm")
                                          (lambda (p)
                                            (let ((l (read-line p)))
                                              (if (eof-object? l) "" l)))))
                                      (lambda _ ""))))
                            (comms (map comm pids)))
                       (when (member "td-builder" comms)
                         (error "LEAK: the launching `td-builder' is visible in /proc — build is NOT in its own pid namespace; comms=" comms))
                       ;; Sanity: the fresh procfs is live (our own pid is listed).
                       (unless (member (number->string (getpid)) pids)
                         (error "own pid absent from /proc — no procfs for this pid namespace")))
                     (mkdir #$output))))))
    (build-derivations store (list drv))
    (format #t "PROBE_DRV=~a~%" (derivation-file-name drv))
    (format #t "PROBE_OUT=~a~%" (derivation->output-path drv))))
