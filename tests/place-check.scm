;; tests/place-check.scm — M10.2 placer artifact validation.
;;
;; The Makefile `place` rung builds + `--check`s a placed target tree (produced by
;; the guix-free placer over Guix-built bootc images) and hands its store path to
;; THIS script via TD_PLACED. We crack the tree and assert the placer's contract,
;; against the SAME per-generation root labels the typed compiler derives (so the
;; assertion cannot drift from the implementation):
;;
;;   (1a) PLACED       — each present generation N has boot/td/gen-N/{bzImage,
;;        initrd.cpio.gz}, both non-empty, plus a root-label file == that
;;        generation's own label (td-root-gen-N).
;;   (1b) ROOT CONTENT — each present generation's APPLIED userspace root is staged
;;        at roots/td/gen-N/root.tar, non-empty — so root=LABEL=td-root-gen-N refers
;;        to a root that actually exists (M10.3 writes it onto a labeled fs).
;;   (2)  PER-GEN ROOT — present generations' initrds are pairwise DISTINCT (each
;;        carries its own generation's root — the M10 crux; a shared initrd would
;;        mean rollback boots the same filesystem).
;;   (3)  MENU         — grub.cfg has the placer's marker-delimited managed block
;;        with EXACTLY one menuentry per present generation, and gen-N's directives
;;        live INSIDE gen-N's OWN entry: that entry loads gen-N's kernel/initrd and
;;        selects gen-N's root, and contains NO other generation's directives (so
;;        swapping initrd/root directives BETWEEN entries fails — a block-wide
;;        substring search would have passed it).
;;   (4)  PRESERVED    — the user's grub.cfg preamble (outside the markers) survives.
;;   (5)  PRUNED       — each absent generation (TD_ABSENT) has NO boot dir, NO root
;;        content dir, AND NO menu reference at all.
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
(define (gen-dir n)  (under "boot" "td" (string-append "gen-" n)))
(define (root-dir n) (under "roots" "td" (string-append "gen-" n)))
(define (root-tar n) (string-append (root-dir n) "/root.tar"))
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

;; --- parse the managed block into individual menuentries -----------------------
;; Each entry is `menuentry "td generation N (...)" { ... }`. We split on the entry
;; header and take each entry's body up to its closing `}`, so a directive can be
;; attributed to the ONE entry it belongs to (not just "somewhere in the block").
(define entry-prefix "menuentry \"td generation ")

(define (leading-int s)                 ;the run of digits at the start of S
  (let loop ((i 0))
    (if (and (< i (string-length s)) (char-numeric? (string-ref s i)))
        (loop (+ i 1))
        (substring s 0 i))))

(define menuentries                     ;list of (gen-string . body-text)
  (if (not managed-block)
      '()
      (let loop ((i 0) (acc '()))
        (let ((s (string-contains managed-block entry-prefix i)))
          (if (not s)
              (reverse acc)
              (let* ((close (string-contains managed-block "}" s))
                     (end   (if close (+ close 1) (string-length managed-block)))
                     (body  (substring managed-block s end))
                     (g     (leading-int
                             (substring managed-block
                                        (+ s (string-length entry-prefix))))))
                (loop end (cons (cons g body) acc))))))))

(format #t "~%== M10.2 placer artifact validation ==~%")
(format #t "  tree=~a~%  present=~s  absent=~s~%" tree present absent)

;; (4) user preamble preserved
(unless (string-contains grub "set timeout=5")
  (fail "the user grub.cfg preamble (set timeout=5) was not preserved"))

;; managed block present and well-formed
(unless managed-block
  (fail "grub.cfg has no well-formed td-place managed block (markers missing/reordered)"))

;; (3) exactly one menuentry per present generation
(when managed-block
  (unless (= (length menuentries) (length present))
    (fail "managed block has ~a menuentries, expected ~a (one per present generation)"
          (length menuentries) (length present))))

;; (1a) per present generation: placed kernel/initrd/root-label
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
                 n recorded label)))))))
 present)

;; (1b) per present generation: the applied userspace root CONTENT is staged
(for-each
 (lambda (n)
   (let ((rt (root-tar n)))
     (cond
      ((not (file-exists? rt))
       (fail "generation ~a: no applied root content placed (~a missing)" n rt))
      ((zero? (stat:size (stat rt)))
       (fail "generation ~a: applied root content is empty (~a)" n rt)))))
 present)

;; (3) each generation's directives live INSIDE its OWN menuentry, and no foreign
;; generation's directives appear there. Root labels are matched with a trailing
;; space (the menu writes `root=LABEL=<label> quiet`) so gen-1 is not a prefix hit
;; inside gen-10.
(for-each
 (lambda (n)
   (let ((mine (filter (lambda (e) (string=? (car e) n)) menuentries)))
     (cond
      ((not (= (length mine) 1))
       (fail "generation ~a: expected exactly one menuentry, found ~a" n (length mine)))
      (else
       (let ((body  (cdr (car mine)))
             (lk    (format #f "/td/gen-~a/bzImage" n))
             (li    (format #f "/td/gen-~a/initrd.cpio.gz" n))
             (lr    (format #f "root=LABEL=~a " (expected-label n))))
         (unless (string-contains body lk)
           (fail "generation ~a: its menuentry does not load its kernel (~a)" n lk))
         (unless (string-contains body li)
           (fail "generation ~a: its menuentry does not load its initrd (~a)" n li))
         (unless (string-contains body lr)
           (fail "generation ~a: its menuentry does not select its own root (root=LABEL=~a)"
                 n (expected-label n)))
         ;; NO OTHER present generation's directives may appear in THIS entry
         (for-each
          (lambda (m)
            (unless (string=? m n)
              (let ((fk (format #f "/td/gen-~a/" m))
                    (fr (format #f "root=LABEL=~a " (expected-label m))))
                (when (string-contains body fk)
                  (fail "generation ~a's menuentry references generation ~a's files (~a) — directives crossed entries"
                        n m fk))
                (when (string-contains body fr)
                  (fail "generation ~a's menuentry selects generation ~a's root (~a) — directives crossed entries"
                        n m (expected-label m))))))
          present))))))
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

;; (5) pruned generations: no boot dir, no root content dir, no menu reference
(for-each
 (lambda (n)
   (when (file-exists? (gen-dir n))
     (fail "generation ~a was supposed to be pruned but its boot dir still exists" n))
   (when (file-exists? (root-dir n))
     (fail "generation ~a was supposed to be pruned but its root content dir still exists" n))
   (when (string-contains grub (format #f "/td/gen-~a/" n))
     (fail "generation ~a was supposed to be pruned but a menu entry still references it" n)))
 absent)

(if (zero? failures)
    (begin
      (format #t "PASS: every present generation is placed with its own kernel/initrd, \
its applied root content, and a menuentry that selects its own root and no other's; \
the user preamble is preserved; pruned generations leave no boot dir, no root \
content, and no menu entry.~%")
      (exit 0))
    (begin
      (format #t "~a check(s) failed.~%" failures)
      (exit 1)))
