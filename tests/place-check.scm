;; tests/place-check.scm — M10.2 placer artifact validation.
;;
;; The Makefile `place` rung builds + `--check`s a placed target tree (produced by
;; the guix-free placer over Guix-built bootc images) and hands its store path to
;; THIS script via TD_PLACED. We crack the tree and assert the placer's contract,
;; against the SAME per-generation root labels the typed compiler derives (so the
;; assertion cannot drift from the implementation):
;;
;;   (1) PLACED      — each present generation N has boot/td/gen-N/{bzImage,
;;       initrd.cpio.gz}, both non-empty, plus a root-label file == that
;;       generation's own label (td-root-gen-N).
;;   (2) PER-GEN ROOT — present generations' initrds are pairwise DISTINCT (each
;;       carries its own generation's root — the M10 crux; a shared initrd would
;;       mean rollback boots the same filesystem).
;;   (3) MENU         — grub.cfg has the placer's marker-delimited managed block
;;       with EXACTLY one menuentry per present generation, and gen-N's entry
;;       points `linux`/`initrd` at THAT generation's placed files and selects
;;       THAT generation's root (root=LABEL=td-root-gen-N) — distinct per entry.
;;   (4) PRESERVED    — the user's grub.cfg preamble (outside the markers) survives.
;;   (5) PRUNED       — each absent generation (TD_ABSENT) has NO per-generation
;;       root dir AND NO menu reference at all (dir + entry both gone).
;;
;; Reusable across scenarios via env: TD_PLACED (tree path), TD_PRESENT (space-sep
;; generations expected present), TD_ABSENT (space-sep generations expected pruned).
;; Run via `guix repl` so (system td-typed) is on the load path. Exits non-zero on
;; any failure.
(use-modules (rnrs io ports)
             (rnrs bytevectors)
             (ice-9 format)
             (srfi srfi-1)
             (srfi srfi-13)
             (system td-typed))

(define failures 0)
(define (fail fmt . args)
  (set! failures (+ failures 1))
  (apply format #t (string-append "FAIL: " fmt "~%") args))

(define (must k)
  (or (getenv k) (begin (format #t "FAIL: env ~a not set~%" k) (exit 1))))

(define (gens-of k)
  (filter (lambda (s) (not (string-null? s)))
          (string-split (or (getenv k) "") #\space)))

(define tree    (must "TD_PLACED"))
(define present (gens-of "TD_PRESENT"))   ;list of generation strings
(define absent  (gens-of "TD_ABSENT"))

(define (under . parts) (string-join (cons tree parts) "/"))
(define (gen-dir n) (under "boot" "td" (string-append "gen-" n)))
(define (slurp f) (call-with-input-file f get-string-all))
(define (slurp-bytes f) (call-with-input-file f get-bytevector-all))

;; The expected per-generation root label — from the SAME typed compiler the
;; images were built with, so the assertion cannot drift.
(define (expected-label n)
  (td-config-effective-root-label (td-config #:generation (string->number n))))

;; The placer's managed-block markers (kept in sync with system/td-place.sh).
(define begin-mark "# >>> td generations (managed by td-place) >>>")
(define end-mark   "# <<< td generations (managed by td-place) <<<")

(define grub (slurp (under "boot" "grub" "grub.cfg")))

;; The substring of grub.cfg strictly between the managed markers (or #f).
(define managed-block
  (let ((b (string-contains grub begin-mark))
        (e (string-contains grub end-mark)))
    (and b e (> e b)
         (substring grub (+ b (string-length begin-mark)) e))))

(define (count-substring hay needle)
  (let loop ((i 0) (n 0))
    (let ((hit (string-contains hay needle i)))
      (if hit (loop (+ hit (string-length needle)) (+ n 1)) n))))

(format #t "~%== M10.2 placer artifact validation ==~%")
(format #t "  tree=~a~%  present=~s  absent=~s~%" tree present absent)

;; (4) user preamble preserved
(unless (string-contains grub "set timeout=5")
  (fail "the user grub.cfg preamble (set timeout=5) was not preserved"))

;; managed block present and well-formed
(unless managed-block
  (fail "grub.cfg has no well-formed td-place managed block (markers missing/reordered)"))

;; (3) exactly one menuentry per present generation, inside the managed block
(when managed-block
  (let ((n (count-substring managed-block "menuentry \"td generation ")))
    (unless (= n (length present))
      (fail "managed block has ~a menuentries, expected ~a (one per present generation)"
            n (length present)))))

;; (1)(2)(3) per present generation
(for-each
 (lambda (n)
   (let* ((d      (gen-dir n))
          (kf     (string-append d "/bzImage"))
          (initrd (string-append d "/initrd.cpio.gz"))
          (lf     (string-append d "/root-label"))
          (label  (expected-label n)))
     (cond
      ((not (and (file-exists? kf) (file-exists? initrd) (file-exists? lf)))
       (fail "generation ~a: missing placed bzImage/initrd/root-label under ~a" n d))
      (else
       (when (zero? (stat:size (stat kf)))
         (fail "generation ~a: placed bzImage is empty" n))
       (when (zero? (stat:size (stat initrd)))
         (fail "generation ~a: placed initrd is empty" n))
       (let ((recorded (string-trim-right (slurp lf))))
         (unless (string=? recorded label)
           (fail "generation ~a: recorded root-label ~s != expected ~s"
                 n recorded label)))
       ;; (3) the menu entry points at THIS generation's files + root
       (let ((lk (format #f "/td/gen-~a/bzImage" n))
             (li (format #f "/td/gen-~a/initrd.cpio.gz" n))
             (lr (format #f "root=LABEL=~a" label)))
         (when managed-block
           (unless (string-contains managed-block lk)
             (fail "generation ~a: menu does not load its kernel (~a)" n lk))
           (unless (string-contains managed-block li)
             (fail "generation ~a: menu does not load its initrd (~a)" n li))
           (unless (string-contains managed-block lr)
             (fail "generation ~a: menu does not select its own root (~a)" n lr))))))))
 present)

;; (2) present generations' initrds are pairwise distinct (per-generation root)
(let loop ((ps present))
  (unless (or (null? ps) (null? (cdr ps)))
    (let ((a (string-append (gen-dir (car ps)) "/initrd.cpio.gz")))
      (when (file-exists? a)
        (for-each
         (lambda (m)
           (let ((b (string-append (gen-dir m) "/initrd.cpio.gz")))
             (when (and (file-exists? b)
                        (bytevector=? (slurp-bytes a) (slurp-bytes b)))
               (fail "generations ~a and ~a have IDENTICAL initrds — not per-generation roots"
                     (car ps) m))))
         (cdr ps))))
    (loop (cdr ps))))

;; (5) pruned generations: no dir, no menu reference
(for-each
 (lambda (n)
   (when (file-exists? (gen-dir n))
     (fail "generation ~a was supposed to be pruned but its root dir still exists" n))
   (when (string-contains grub (format #f "/td/gen-~a/" n))
     (fail "generation ~a was supposed to be pruned but a menu entry still references it" n)))
 absent)

(if (zero? failures)
    (begin
      (format #t "PASS: every present generation is placed with its own kernel/initrd \
and a menu entry selecting its own root; the user preamble is preserved; \
pruned generations leave no root dir and no menu entry.~%")
      (exit 0))
    (begin
      (format #t "~a check(s) failed.~%" failures)
      (exit 1)))
