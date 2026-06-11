;; tests/td-builder-nar.scm — S2 oracle pairs for the td-builder rung's NAR
;; differential (plan/td-builder.md). Prints one line per store item:
;;
;;   NAR=<store path> <base16 sha256 of its NAR>
;;
;; The hash comes from the DAEMON's own database (query-path-info), i.e. the
;; oracle's recorded NAR hash — not recomputed here — so the rung compares
;; td-builder's serialization against what guix-daemon itself registered
;; (prime directive 4). Two items:
;;
;;   1. a constructed fixture covering every NAR node type and the encoding
;;      edges: regular, EMPTY regular, executable, symlink (target written
;;      verbatim, never resolved — it dangles on purpose), nested dir, EMPTY
;;      dir, sort-stress names ("B" < "a" in codepoint order; "a-b" between
;;      "a" and "ab" catches length-vs-prefix mistakes), and contents of
;;      lengths 0/3/8/9 exercising the pad-to-8 framing;
;;   2. td-builder's own package output — a real store item.
;;
;; Unlike the sibling drv scripts this one REALISES inside the repl
;; (build-derivations — the fixture is tiny and td-builder is already built by
;; the rung's S1 leg): exit honesty still holds because `guix repl FILE`
;; propagates a failing script's status (see tests/eval.scm), and the rung
;; additionally guards against exit-0-but-empty output (`test -n` + a pair
;; count). Comparison and pass/fail live in the Makefile rung.
(use-modules (guix store)
             (guix derivations)
             (guix gexp)
             (guix monads)
             (guix base16)
             (ice-9 format)
             (system td-builder))

(define %nar-fixture
  (computed-file "td-nar-fixture"
    #~(begin
        (mkdir #$output)
        ;; sort stress: codepoint order is B a a-b ab; a case-insensitive or
        ;; locale collation orders them differently.
        (call-with-output-file (string-append #$output "/a")
          (lambda (port) (display "abc" port)))          ;3 bytes -> 5 pad
        (call-with-output-file (string-append #$output "/B")
          (lambda (port) (display "12345678" port)))     ;8 bytes -> 0 pad
        (call-with-output-file (string-append #$output "/ab")
          (lambda (port) (display "123456789" port)))    ;9 bytes -> 7 pad
        (call-with-output-file (string-append #$output "/a-b")
          (lambda (port) #t))                            ;empty regular
        (let ((exe (string-append #$output "/run")))
          (call-with-output-file exe
            (lambda (port) (display "#!/bin/sh\n" port)))
          (chmod exe #o755))
        (symlink "a" (string-append #$output "/link"))
        (symlink "no-such-target" (string-append #$output "/dangling"))
        (mkdir (string-append #$output "/sub"))
        (call-with-output-file (string-append #$output "/sub/inner")
          (lambda (port) (display "x" port)))
        (mkdir (string-append #$output "/sub/empty")))))

(with-store store
  ;; Offline contract, as in every sibling: no substitutes, no offloading.
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (for-each
   (lambda (obj)
     (let ((drv (run-with-store store (lower-object obj))))
       (build-derivations store (list drv))
       (let* ((out (derivation->output-path drv))
              (info (query-path-info store out)))
         (format #t "NAR=~a ~a~%" out
                 (bytevector->base16-string (path-info-hash info))))))
   (list %nar-fixture td-builder)))
