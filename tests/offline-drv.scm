;; tests/offline-drv.scm — offline-isolation: print the two network-namespace
;; probe derivations, so the Makefile's `offline` rung can realise them and
;; `guix build --check` them (forcing the probe assertions to RE-EXECUTE every
;; loop — a probe that ran green once is not a standing guarantee).
;;
;; One probe body, two derivations; the ONLY difference is fixed-output-ness,
;; which is exactly the daemon's network-isolation lever:
;;
;;   • DRV_SANDBOX — a regular (non-fixed-output) derivation. guix-daemon gives
;;     it a private netns, so this is the "deliberate undeclared fetch" of the
;;     track's acceptance: a builder that was NOT declared fixed-output tries to
;;     reach the network and must fail. Green on any correctly sandboxed daemon.
;;
;;   • DRV_DAEMON — the same body as a fixed-output derivation. FO builders are
;;     the one path the daemon does NOT unshare a netns for: they run in the
;;     DAEMON'S OWN netns — the same netns `guix substitute` queries from. So
;;     "only loopback here" asserts the daemon itself is network-isolated and a
;;     cold path cannot even query. RED until the host wraps guix-daemon in its
;;     own netns (plan/offline-isolation.md S3) — and that red is the
;;     verified-red for the shared probe body: identical assertions, network
;;     present, builder fails.
;;
;; The probe asserts the netns two ways: /proc/net/dev must list ONLY `lo`
;; (interface visibility is precisely what CLONE_NEWNET scopes — check.sh's
;; host-side control proves this mechanism reports non-lo interfaces where
;; network IS present), and an actual TCP egress attempt must raise (the
;; interface check runs first, so a network-visible red fails fast instead of
;; hanging in connect). Output is a fixed one-line file, so both probes are
;; trivially reproducible and `--check` doubles as the per-loop re-run.
;;
;; Emits `DRV_SANDBOX=` / `DRV_DAEMON=` lines (the Makefile greps them out),
;; mirroring the two-step lower-then-realise pattern: lowering only prints drv
;; names; the build — and the rung's honest pass/fail — happens in `guix build`.
(use-modules (guix store)
             (guix derivations)
             (guix gexp)
             (guix monads)
             (guix base32)
             (ice-9 format))

(define (netns-probe-gexp context)
  ;; CONTEXT is only a label baked into the messages (and it usefully makes the
  ;; two probes distinct derivations).
  #~(begin
      (use-modules (ice-9 rdelim))
      ;; Every interface in THIS process's netns, parsed from /proc/net/dev
      ;; (interface lines are "  name: ..."; the two header lines have no colon).
      (define names
        (call-with-input-file "/proc/net/dev"
          (lambda (port)
            (let loop ((line (read-line port)) (acc '()))
              (if (eof-object? line)
                  (reverse acc)
                  (loop (read-line port)
                        (let ((colon (string-index line #\:)))
                          (if colon
                              (cons (string-trim-both (substring line 0 colon))
                                    acc)
                              acc))))))))
      (format #t "~a probe: netns interfaces: ~s~%" #$context names)
      (unless (equal? names '("lo"))
        (format (current-error-port)
                "FAIL: the ~a netns sees non-loopback interfaces ~s — an \
undeclared fetch could reach the network from a path that must be isolated.~%"
                #$context names)
        (exit 1))
      ;; Belt and suspenders: an actual egress attempt (TCP to TEST-NET-1, a
      ;; documentation address no real host answers) must raise immediately in
      ;; a loopback-only netns (no route).
      (let ((sock (socket AF_INET SOCK_STREAM 0)))
        (catch 'system-error
          (lambda ()
            (connect sock AF_INET (inet-pton AF_INET "192.0.2.1") 9)
            (format (current-error-port)
                    "FAIL: a TCP connect out of the ~a netns SUCCEEDED — \
egress is possible.~%"
                    #$context)
            (exit 1))
          (lambda args
            (format #t "~a probe: egress attempt failed as required: ~a~%"
                    #$context (strerror (system-error-errno args))))))
      (call-with-output-file #$output
        (lambda (port) (display "isolated\n" port)))))

(with-store store
  ;; Offline contract: forbid substitutes AND remote offloading for this store
  ;; session (`guix repl` does not read GUIX_BUILD_OPTIONS).
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (let ((sandbox (run-with-store store
                   (gexp->derivation "td-offline-sandbox-probe"
                                     (netns-probe-gexp "sandbox"))))
        ;; sha256 of the literal "isolated\n" the probe writes — what makes
        ;; this one fixed-output, i.e. run in the daemon's own netns.
        (daemon (run-with-store store
                  (gexp->derivation "td-offline-daemon-probe"
                                    (netns-probe-gexp "daemon")
                                    #:hash-algo 'sha256
                                    #:hash (nix-base32-string->bytevector
                                            "0rnc30gw1l688ijhhfjka569ia9c8flsvh8cg538fikfkfa7g3cv")
                                    #:recursive? #f))))
    (format #t "DRV_SANDBOX=~a~%" (derivation-file-name sandbox))
    (format #t "DRV_DAEMON=~a~%" (derivation-file-name daemon))))
