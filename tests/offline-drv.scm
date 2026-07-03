;; tests/offline-drv.scm — offline-isolation probe for td's OWN builder: print
;; the DRV_SANDBOX network-namespace probe derivation the `td-offline` gate
;; (mk/gates/360) realizes with `td-builder realize`. The probe is a regular
;; (non-fixed-output) derivation whose builder asserts, from INSIDE the build:
;; /proc/net/dev lists ONLY `lo`, and an actual TCP egress attempt raises — so
;; a realize that SUCCEEDS proves td's userns+NEWNET sandbox isolated the build.
;;
;; (The old guix-daemon twin — DRV_DAEMON, the fixed-output variant probing the
;; DAEMON's netns — was retired with the `offline` gate 185 and the guix-system
;; museum tier, human direction 2026-07-02: guix-daemon's sandbox is not td's
;; subject; 360-td-offline covers td's builder, which is.)
;;
;; The probe asserts the netns two ways: interface visibility (precisely what
;; CLONE_NEWNET scopes) and a real TCP egress attempt to TEST-NET-1 (192.0.2.1,
;; a documentation address no real host answers), which must raise immediately
;; in a loopback-only netns. The interface check runs first, so a
;; network-visible red fails fast instead of hanging in connect.
;;
;; Run as a repl SCRIPT (`guix repl FILE`), never piped via STDIN: guix repl
;; reading from STDIN always exits 0 (it swallows the script's status), while
;; script mode propagates a load error as a non-zero exit.
(use-modules (guix store)
             (guix derivations)
             (guix gexp)
             (guix monads)
             (ice-9 format))

(define (netns-probe-gexp context)
  ;; CONTEXT is only a label baked into the messages.
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
                                     (netns-probe-gexp "sandbox")))))
    (format #t "DRV_SANDBOX=~a~%" (derivation-file-name sandbox))))
