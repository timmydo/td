;; tests/check-memo-drvs.scm — fixture derivations for the `memo` rung
;; Lowers (never builds) three TINY derivations and
;; prints their file names:
;;
;;   DRV_DET=...     deterministic fixture A — the memoization subject
;;   DRV_DET2=...    deterministic fixture with the SAME NAME as A but
;;                   different content — a different drv hash behind an
;;                   identical name (the "changed drv can never hit" subject,
;;                   verified-red A's structural twin: a helper that keyed
;;                   verdicts by NAME instead of drv store path would falsely
;;                   hit exactly here)
;;   DRV_NONDET=...  DELIBERATELY nondeterministic: the builder embeds
;;                   gettimeofday microseconds, so a --check rebuild can
;;                   never be bit-identical. The rung's leg D instrument
;;                   (detection power intact on a miss): the helper, given
;;                   this drv with no verdict, must run the real --check and
;;                   go RED. Its verdict must never exist.
;;
;; Run as a repl SCRIPT (not piped) for an honest exit status, as in the
;; sibling drv scripts.
(use-modules (guix)
             (guix gexp)
             (guix monads)
             (guix store)
             (guix derivations)
             (ice-9 format))

(define det
  (computed-file "td-memo-det-a"
    #~(call-with-output-file #$output
        (lambda (port)
          (display "td check-memo deterministic fixture A\n" port)))))

(define det2
  ;; Same name as `det`, different content: only the drv HASH distinguishes
  ;; them — exactly what constraint 1's content-addressed keying must see.
  (computed-file "td-memo-det-a"
    #~(call-with-output-file #$output
        (lambda (port)
          (display "td check-memo deterministic fixture A'\n" port)))))

(define nondet
  (computed-file "td-memo-nondet"
    #~(call-with-output-file #$output
        (lambda (port)
          ;; Microsecond wall-clock: two builds of this drv can in practice
          ;; never write the same bytes, so `guix build --check` always reds.
          (let ((t (gettimeofday)))
            (display (car t) port)
            (display "." port)
            (display (cdr t) port)
            (newline port))))))

(with-store store
  ;; Offline contract, as in every sibling: no substitutes, no offloading.
  (set-build-options store #:use-substitutes? #f #:offload? #f)
  (for-each
   (lambda (label obj)
     (format #t "~a=~a~%" label
             (derivation-file-name
              (run-with-store store (lower-object obj)))))
   (list "DRV_DET" "DRV_DET2" "DRV_NONDET")
   (list det det2 nondet)))
