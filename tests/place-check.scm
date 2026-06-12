;; tests/place-check.scm — M10.2/M10.3 placer artifact validation.
;;
;; The Makefile `place` rung (and the M10.3 `rollback` rung, for its mkfs'd tree)
;; builds + `--check`s a placed target tree (produced by the guix-free placer over
;; Guix-built bootc images) and hands its store path to THIS script via TD_PLACED.
;; We crack the tree and assert the placer's contract, against the SAME
;; per-generation root labels the typed compiler derives (so the assertion cannot
;; drift from the implementation):
;;
;;   (1a) PLACED       — each present generation N has boot/td/gen-N/{bzImage,
;;        initrd.cpio.gz}, both non-empty, plus root-label/system/root-uuid files
;;        that match BOTH the expected values and the generation's own placed
;;        td-identity (the on-disk state the menu is regenerated from is bound to
;;        the image identity). M12 (§2.7): the placed td-identity additionally
;;        records what the image IS — `image-digest=sha256:<hex>`, computed by
;;        the placer over the artifact it actually unpacked. The line must be a
;;        well-formed sha256, and, when TD_IMAGES maps the generation to its
;;        artifact ("N=/gnu/store/... ..."), must EQUAL that artifact's actual
;;        sha256 — the placer hashed what it placed, not something else.
;;   (1b) ROOT CONTENT — each present generation's APPLIED userspace root is staged
;;        at roots/td/gen-N/root.tar, non-empty — so the menu's root=td-root-gen-N
;;        refers to a root that actually exists.
;;   (1c) LIVE ROOT FS — with TD_MKFS=1, roots/td/gen-N/root.img is a real ext4
;;        filesystem: superblock magic 0xEF53, volume LABEL == td-root-gen-N and
;;        UUID == the identity's deterministic root-uuid, read STRAIGHT FROM THE
;;        SUPERBLOCK bytes (offsets 1024+0x38/0x68/0x78) — no tools, no mounting.
;;   (1d) VERITY (M11) — with TD_MKFS=1, each generation also records its
;;        dm-verity root hash + hash offset (boot/td/gen-N/verity-roothash,
;;        verity-hashoffset), and root.img carries the APPENDED hash tree: a
;;        well-formed verity superblock at the recorded offset (magic, version,
;;        identity UUID, sha256, 4096-byte blocks, the fixed td salt) whose
;;        data_blocks cover EXACTLY the ext4 data area — all read straight from
;;        the bytes, no tools.
;;   (2)  PER-GEN ROOT — present generations' initrds are pairwise DISTINCT (each
;;        carries its own generation's root — the M10 crux; a shared initrd would
;;        mean rollback boots the same filesystem).
;;   (3)  MENU         — grub.cfg has the placer's marker-delimited managed block
;;        with EXACTLY one menuentry per present generation (each carrying its
;;        `--id td-gen-N`), and gen-N's directives live INSIDE gen-N's OWN entry:
;;        that entry loads gen-N's kernel/initrd, selects gen-N's root — M11:
;;        for an mkfs tree, that generation's RECORDED td.roothash=/
;;        td.hashoffset= and NO root= argument at all (cryptographic root
;;        selection; "/" is a declared tmpfs); for an mkfs-less tree the
;;        legacy bare-label root= — and BOOTS gen-N's system
;;        (gnu.system=<its identity's system path> gnu.load=<that path>/boot)
;;        — and contains NO other generation's directives (file paths, root
;;        label, root hash, or system path).
;;   (3b) BOOT WIRING  — the managed block sets `default=td-gen-<newest present>`,
;;        carries the manual-rollback hook (`if [ -s /td/default.cfg ]; then
;;        source /td/default.cfg; fi` — the file the rollback ACT writes), and,
;;        when TD_BOOT_LABEL is set, a `search --no-floppy --label <label>
;;        --set=root` line selecting the boot partition.
;;   (4)  PRESERVED    — the user's grub.cfg preamble (outside the markers) survives.
;;   (5)  PRUNED       — each absent generation (TD_ABSENT) has NO boot dir, NO root
;;        content dir, AND NO menu reference at all.
;;
;; Reusable across scenarios via env: TD_PLACED (tree path), TD_PRESENT (space-sep
;; generations expected present), TD_ABSENT (space-sep generations expected pruned),
;; TD_MKFS (assert live root filesystems), TD_BOOT_LABEL (assert the search line),
;; TD_IMAGES (space-sep "N=<artifact>" — assert each recorded image-digest equals
;; that artifact's sha256; unmapped generations get the well-formedness check only).
;; Run via `guix repl` so (system td-typed) is on the load path. Exits non-zero on
;; any failure.
(use-modules (rnrs io ports)
             (rnrs bytevectors)
             (ice-9 format)
             (srfi srfi-1)
             (srfi srfi-13)
             (gcrypt hash)
             (gcrypt base16)
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
(define mkfs?   (equal? (getenv "TD_MKFS") "1"))
(define boot-label (getenv "TD_BOOT_LABEL"))  ;#f = no search-line assertion

;; TD_IMAGES: "N=<artifact path>" or "N=sha256:<hex>" pairs — generation ->
;; what the placed image-digest must equal: the sha256 of that artifact file
;; (legacy --image placement) or that literal digest (verified --registry
;; placement, where the §2.7 identity is the manifest digest). Empty =
;; format-only.
(define images
  (filter-map (lambda (s)
                (let ((i (string-index s #\=)))
                  (and i (cons (substring s 0 i) (substring s (+ i 1))))))
              (gens-of "TD_IMAGES")))

(define (under . parts) (string-join (cons tree parts) "/"))
(define (gen-dir n)  (under "boot" "td" (string-append "gen-" n)))
(define (root-dir n) (under "roots" "td" (string-append "gen-" n)))
(define (root-tar n) (string-append (root-dir n) "/root.tar"))
(define (root-img n) (string-append (root-dir n) "/root.img"))
(define (slurp f) (call-with-input-file f get-string-all))
(define (slurp-bytes f) (call-with-input-file f get-bytevector-all))
(define (slurp-line f) (string-trim-right (slurp f)))

;; The expected per-generation root label — from the SAME typed compiler the
;; images were built with, so the assertion cannot drift.
(define (expected-label n)
  (td-config-effective-root-label (td-config #:generation (string->number n))))

;; Parse a placed boot/td/gen-N/td-identity ("key=value" lines) into an alist.
(define (parse-identity file)
  (filter-map (lambda (line)
                (let ((i (string-index line #\=)))
                  (and i (cons (substring line 0 i) (substring line (+ i 1))))))
              (string-split (slurp file) #\newline)))

(define (hex-string? s)
  (and (string? s)
       (string-every (lambda (c) (string-index "0123456789abcdef" c)) s)))

;; A well-formed §2.7 identity digest: "sha256:" + 64 hex chars.
(define (well-formed-digest? s)
  (and (string? s)
       (string-prefix? "sha256:" s)
       (= (string-length s) (+ 7 64))
       (hex-string? (substring s 7))))

;; The §2.7 identity of an artifact file: sha256 over its bytes, as the
;; placer must have computed it.
(define (artifact-digest file)
  (string-append "sha256:"
                 (bytevector->base16-string
                  (file-hash (hash-algorithm sha256) file))))

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
;; Each entry is `menuentry "td generation N (...)" --id td-gen-N { ... }`. We
;; split on the entry header and take each entry's body up to its closing `}`, so
;; a directive can be attributed to the ONE entry it belongs to (not just
;; "somewhere in the block").
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

(format #t "~%== M10.2/M10.3 placer artifact validation ==~%")
(format #t "  tree=~a~%  present=~s  absent=~s  mkfs=~a  boot-label=~s~%"
        tree present absent mkfs? boot-label)

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

;; (3b) boot wiring: default to the newest present generation; the manual-rollback
;; hook; the boot-partition search line when a boot label is expected.
(when managed-block
  (let ((newest (and (pair? present)
                     (number->string
                      (apply max (map string->number present))))))
    (when newest
      (unless (string-contains managed-block
                               (format #f "set default=td-gen-~a" newest))
        (fail "managed block does not default to the newest present generation (set default=td-gen-~a)"
              newest)))
    (unless (string-contains managed-block
                             "if [ -s /td/default.cfg ]; then source /td/default.cfg; fi")
      (fail "managed block lacks the manual-rollback hook (source /td/default.cfg)"))
    (when boot-label
      (unless (string-contains managed-block
                               (format #f "search --no-floppy --label ~a --set=root"
                                       boot-label))
        (fail "managed block lacks the boot-partition search line for label ~s"
              boot-label)))))

;; (1a) per present generation: placed kernel/initrd + self-describing state bound
;; to the placed identity (root-label, system, root-uuid).
(for-each
 (lambda (n)
   (let* ((d      (gen-dir n))
          (kf     (string-append d "/bzImage"))
          (initrd (string-append d "/initrd.cpio.gz"))
          (lf     (string-append d "/root-label"))
          (sf     (string-append d "/system"))
          (uf     (string-append d "/root-uuid"))
          (idf    (string-append d "/td-identity"))
          (label  (expected-label n)))
     (cond
      ((not (and (file-exists? kf) (file-exists? initrd) (file-exists? lf)
                 (file-exists? sf) (file-exists? uf) (file-exists? idf)))
       (fail "generation ~a: missing placed bzImage/initrd/root-label/system/root-uuid/td-identity under ~a" n d))
      (else
       (when (zero? (stat:size (stat kf)))
         (fail "generation ~a: placed bzImage is empty" n))
       (when (zero? (stat:size (stat initrd)))
         (fail "generation ~a: placed initrd is empty" n))
       (let ((recorded (slurp-line lf)))
         (unless (string=? recorded label)
           (fail "generation ~a: recorded root-label ~s != expected ~s"
                 n recorded label)))
       (let* ((id      (parse-identity idf))
              (id-sys  (assoc-ref id "system"))
              (id-uuid (assoc-ref id "root-uuid"))
              (sys     (slurp-line sf))
              (uuid    (slurp-line uf)))
         (unless (and (string? id-sys) (string-prefix? "/gnu/store/" id-sys))
           (fail "generation ~a: placed td-identity has no usable system= field" n))
         (unless (equal? sys id-sys)
           (fail "generation ~a: recorded system ~s != identity system ~s"
                 n sys id-sys))
         (unless (and (string? id-uuid) (equal? uuid id-uuid))
           (fail "generation ~a: recorded root-uuid ~s != identity root-uuid ~s"
                 n uuid id-uuid))
         ;; M12 (§2.7): the placed identity states what the image IS — the
         ;; sha256 of the artifact the placer unpacked. EXACTLY ONE line (a
         ;; second one — e.g. smuggled in via the embedded identity, which the
         ;; placer rejects — could shadow the placer's own and feed signature
         ;; verification a forged digest); format always; VALUE against the
         ;; actual artifact when TD_IMAGES maps this generation.
         (let ((digests (filter (lambda (kv) (string=? (car kv) "image-digest"))
                                id))
               (digest  (assoc-ref id "image-digest")))
           (cond
            ((not (= (length digests) 1))
             (fail "generation ~a: placed td-identity carries ~a image-digest lines, expected exactly one"
                   n (length digests)))
            ((not (well-formed-digest? digest))
             (fail "generation ~a: placed td-identity carries no well-formed image-digest=sha256:<64-hex> line (got ~s) — the §2.7 identity anchor is missing"
                   n digest))
            ((assoc-ref images n)
             => (lambda (expected)
                  (let ((actual (if (string-prefix? "sha256:" expected)
                                    expected
                                    (artifact-digest expected))))
                    (unless (string=? digest actual)
                      (fail "generation ~a: recorded image-digest ~s != the placed artifact's identity ~a (from ~a) — the placer did not record what it placed"
                            n digest actual expected))))))))))))
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

;; (1c) with TD_MKFS=1: the root content is a LIVE ext4 filesystem whose
;; label/UUID — read straight from the superblock, no tools — are this
;; generation's. ext4 superblock: 1024 bytes in; magic 0xEF53 at +0x38 (LE),
;; s_uuid 16 bytes at +0x68, s_volume_name 16 bytes (NUL-padded) at +0x78.
(define (superblock-bytes img off len)
  (call-with-input-file img
    (lambda (p)
      (seek p (+ 1024 off) SEEK_SET)
      (get-bytevector-n p len))
    #:binary #t))

(define (bytes->uuid-string bv)
  ;; Canonical 8-4-4-4-12 formatting of the raw 16 superblock bytes — ext
  ;; stores the UUID so this equals the string mke2fs -U was given.
  (let ((hex (string-concatenate
              (map (lambda (b) (format #f "~2,'0x" b))
                   (bytevector->u8-list bv)))))
    (string-append (substring hex 0 8) "-" (substring hex 8 12) "-"
                   (substring hex 12 16) "-" (substring hex 16 20) "-"
                   (substring hex 20 32))))

(define (bytes->label bv)
  (let* ((lst (bytevector->u8-list bv))
         (nul (or (list-index zero? lst) (length lst))))
    (list->string (map integer->char (take lst nul)))))

;; Raw bytes at an absolute offset of IMG (the superblock helper above is
;; ext4-superblock-relative; the M11 verity asserts need absolute seeks).
;; Short or EOF reads (a truncated image) yield #f, so every caller below
;; reports a FAIL instead of throwing.
(define (img-bytes img off len)
  (let ((bv (call-with-input-file img
              (lambda (p)
                (seek p off SEEK_SET)
                (get-bytevector-n p len))
              #:binary #t)))
    (and (bytevector? bv) (= (bytevector-length bv) len) bv)))

(define (le-ref img off len)            ;little-endian unsigned int at OFF
  (let ((bv (img-bytes img off len)))
    (and bv
         (let loop ((i (- len 1)) (acc 0))
           (if (< i 0)
               acc
               (loop (- i 1) (+ (* acc 256) (bytevector-u8-ref bv i))))))))

;; M11: the FIXED dm-verity salt — kept in sync with system/td-place.sh
;; (VERITY_SALT), like the managed-block markers above.
(define verity-salt
  "74642d7665726974792d73616c742d7630000000000000000000000000000000")

(define (bytes->hex bv)
  (string-concatenate
   (map (lambda (b) (format #f "~2,'0x" b)) (bytevector->u8-list bv))))

(when mkfs?
  (for-each
   (lambda (n)
     (let ((img (root-img n))
           (label (expected-label n)))
       (cond
        ((not (file-exists? img))
         (fail "generation ~a: TD_MKFS=1 but no live root filesystem (~a missing)" n img))
        ((zero? (stat:size (stat img)))
         (fail "generation ~a: root.img is empty" n))
        (else
         (let ((magic (superblock-bytes img #x38 2)))
           (unless (and (bytevector? magic)
                        (= (bytevector-u8-ref magic 0) #x53)
                        (= (bytevector-u8-ref magic 1) #xEF))
             (fail "generation ~a: root.img has no ext superblock magic (not a filesystem)" n)))
         (let ((sb-label (bytes->label (superblock-bytes img #x78 16))))
           (unless (string=? sb-label label)
             (fail "generation ~a: filesystem LABEL ~s != expected ~s — the menu's root= label would not find it"
                   n sb-label label)))
         (let ((sb-uuid (bytes->uuid-string (superblock-bytes img #x68 16)))
               (id-uuid (assoc-ref (parse-identity
                                    (string-append (gen-dir n) "/td-identity"))
                                   "root-uuid")))
           (unless (equal? sb-uuid id-uuid)
             (fail "generation ~a: filesystem UUID ~s != identity root-uuid ~s"
                   n sb-uuid id-uuid)))))))
   present)

  ;; M11: the appended dm-verity hash area. Each placed generation records the
  ;; root hash + hash offset the placer derived (boot/td/gen-N/verity-roothash,
  ;; verity-hashoffset); root.img must carry a well-formed verity SUPERBLOCK at
  ;; that offset (struct verity_sb: magic "verity\0\0", version 1, uuid at +16,
  ;; algorithm at +32, data/hash block size at +64/+68, data_blocks u64 at +72,
  ;; salt_size u16 at +80, salt at +88) whose parameters are EXACTLY the
  ;; placer's contract: 4096-byte blocks, the identity's UUID, sha256, the
  ;; fixed td salt — and the ext4 DATA area must fill the verity data blocks
  ;; exactly (ext4 size == data_blocks * 4096 == hash offset), so the data the
  ;; hash tree covers is precisely the filesystem the menu boots.
  (for-each
   (lambda (n)
     (let* ((d    (gen-dir n))
            (rh-f (string-append d "/verity-roothash"))
            (ho-f (string-append d "/verity-hashoffset"))
            (img  (root-img n)))
       (cond
        ((not (and (file-exists? rh-f) (file-exists? ho-f)))
         (fail "generation ~a: no recorded verity-roothash/verity-hashoffset — root.img carries no usable integrity metadata" n))
        (else
         (let ((rh (slurp-line rh-f))
               (ho (string->number (slurp-line ho-f))))
           (unless (and (hex-string? rh) (= (string-length rh) 64))
             (fail "generation ~a: recorded verity-roothash ~s is not a sha256 hex digest" n rh))
           (cond
            ((not (and ho (positive? ho) (zero? (remainder ho 4096))))
             (fail "generation ~a: recorded verity-hashoffset ~s is not a positive 4096-multiple" n ho))
            ((not (file-exists? img)) #f)  ;already failed above
            ((not (> (stat:size (stat img)) ho))
             (fail "generation ~a: root.img (~a bytes) has NO appended hash area beyond the recorded offset ~a"
                   n (stat:size (stat img)) ho))
            (else
             (let ((vmagic (img-bytes img ho 8)))
               (unless (and (bytevector? vmagic)
                            (equal? (bytevector->u8-list vmagic)
                                    ;; "verity\0\0"
                                    '(#x76 #x65 #x72 #x69 #x74 #x79 0 0)))
                 (fail "generation ~a: no dm-verity superblock magic at the recorded hash offset ~a" n ho)))
             (unless (equal? (le-ref img (+ ho 8) 4) 1)
               (fail "generation ~a: verity superblock version != 1" n))
             (let ((v-uuid (and=> (img-bytes img (+ ho 16) 16)
                                  bytes->uuid-string))
                   (id-uuid (assoc-ref (parse-identity
                                        (string-append d "/td-identity"))
                                       "root-uuid")))
               (unless (equal? v-uuid id-uuid)
                 (fail "generation ~a: verity superblock UUID ~s != identity root-uuid ~s — the hash tree was not derived for THIS image"
                       n v-uuid id-uuid)))
             (let ((algo (and=> (img-bytes img (+ ho 32) 32) bytes->label)))
               (unless (equal? algo "sha256")
                 (fail "generation ~a: verity hash algorithm ~s != sha256" n algo)))
             (unless (and (equal? (le-ref img (+ ho 64) 4) 4096)
                          (equal? (le-ref img (+ ho 68) 4) 4096))
               (fail "generation ~a: verity data/hash block size != 4096" n))
             (let ((blocks (le-ref img (+ ho 72) 8)))
               (unless (equal? (and blocks (* blocks 4096)) ho)
                 (fail "generation ~a: verity data_blocks ~s * 4096 != hash offset ~a — the hash tree does not cover exactly the data area"
                       n blocks ho)))
             (let ((salt-size (le-ref img (+ ho 80) 2)))
               (unless (equal? salt-size 32)
                 (fail "generation ~a: verity salt size ~s != 32" n salt-size)))
             (let ((salt (and=> (img-bytes img (+ ho 88) 32) bytes->hex)))
               (unless (equal? salt verity-salt)
                 (fail "generation ~a: verity salt is not the fixed td salt — the hash tree is not reproducible by contract" n)))
             ;; the ext4 data area fills the verity data blocks EXACTLY
             (let* ((bs (ash 1024 (or (le-ref img (+ 1024 #x18) 4) 0)))
                    (blocks (le-ref img (+ 1024 #x4) 4))
                    (fs-bytes (and blocks (* blocks bs))))
               (unless (equal? fs-bytes ho)
                 (fail "generation ~a: ext4 size ~s != hash offset ~a — the filesystem and the verity data area disagree"
                       n fs-bytes ho))))))))))
   present))

;; (3) each generation's directives live INSIDE its OWN menuentry, and no foreign
;; generation's directives appear there. The root spec is the BARE label
;; (`root=<label>` — Guix's initrd parses the whole root= value as a label; the
;; dracut-style `LABEL=` prefix would be searched for literally and never match,
;; found the hard way in the disk spike). Labels are matched with a trailing
;; space (`root=<label> gnu.system=...`) so gen-1 is not a prefix hit in gen-10.
(for-each
 (lambda (n)
   (let ((mine (filter (lambda (e) (string=? (car e) n)) menuentries)))
     (cond
      ((not (= (length mine) 1))
       (fail "generation ~a: expected exactly one menuentry, found ~a" n (length mine)))
      (else
       (let* ((body  (cdr (car mine)))
              (sys   (slurp-line (string-append (gen-dir n) "/system")))
              (lk    (format #f "/td/gen-~a/bzImage" n))
              (li    (format #f "/td/gen-~a/initrd.cpio.gz" n))
              (lr    (format #f "root=~a " (expected-label n)))
              (lid   (format #f "--id td-gen-~a " n))
              (lsys  (format #f "gnu.system=~a " sys))
              (lload (format #f "gnu.load=~a/boot" sys)))
         (unless (string-contains body lk)
           (fail "generation ~a: its menuentry does not load its kernel (~a)" n lk))
         (unless (string-contains body li)
           (fail "generation ~a: its menuentry does not load its initrd (~a)" n li))
         ;; Root selection. M11: an mkfs tree's entry carries THIS
         ;; generation's recorded verity root hash + hash offset and NO
         ;; root= argument at all ("/" is a declared tmpfs; the hash on the
         ;; cmdline selects the root cryptographically — a stronger binding
         ;; than the label string ever was). An mkfs-less tree keeps the
         ;; legacy bare-label root= spec (those placements have no live
         ;; root.img and no verity records).
         (if mkfs?
             (let ((rh (format #f "td.roothash=~a "
                               (slurp-line (string-append (gen-dir n)
                                                          "/verity-roothash"))))
                   (ho (format #f "td.hashoffset=~a "
                               (slurp-line (string-append (gen-dir n)
                                                          "/verity-hashoffset")))))
               (unless (string-contains body rh)
                 (fail "generation ~a: its menuentry does not pass its own recorded verity root hash (~a)"
                       n rh))
               (unless (string-contains body ho)
                 (fail "generation ~a: its menuentry does not pass its own recorded verity hash offset (~a)"
                       n ho))
               (when (string-contains body " root=")
                 (fail "generation ~a: its menuentry still passes a root= argument — a verity generation's / is a declared tmpfs; root= would override it"
                       n)))
             (unless (string-contains body lr)
               (fail "generation ~a: its menuentry does not select its own root (root=~a)"
                     n (expected-label n))))
         (unless (string-contains body lid)
           (fail "generation ~a: its menuentry has no --id td-gen-~a (the default/rollback selector)"
                 n n))
         (unless (string-contains body lsys)
           (fail "generation ~a: its menuentry does not pass its own gnu.system (~a)"
                 n sys))
         (unless (string-contains body lload)
           (fail "generation ~a: its menuentry does not gnu.load its own boot script (~a/boot)"
                 n sys))
         ;; NO OTHER present generation's directives may appear in THIS entry
         (for-each
          (lambda (m)
            (unless (string=? m n)
              (let ((fk (format #f "/td/gen-~a/" m))
                    (fr (format #f "root=~a " (expected-label m)))
                    (fs (format #f "gnu.system=~a "
                                (slurp-line (string-append (gen-dir m) "/system")))))
                (when (string-contains body fk)
                  (fail "generation ~a's menuentry references generation ~a's files (~a) — directives crossed entries"
                        n m fk))
                (when (string-contains body fr)
                  (fail "generation ~a's menuentry selects generation ~a's root (~a) — directives crossed entries"
                        n m (expected-label m)))
                ;; M11: nor may it pass a FOREIGN generation's root hash —
                ;; that would boot generation N's files over generation M's
                ;; verified root.
                (when mkfs?
                  (let ((frh (format #f "td.roothash=~a "
                                     (slurp-line
                                      (string-append (gen-dir m)
                                                     "/verity-roothash")))))
                    (when (string-contains body frh)
                      (fail "generation ~a's menuentry passes generation ~a's verity root hash — directives crossed entries"
                            n m))))
                (when (string-contains body fs)
                  (fail "generation ~a's menuentry boots generation ~a's system — directives crossed entries"
                        n m)))))
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
an identity recording its artifact's sha256 (image-digest, §2.7~a), \
its applied root content~a, and a menuentry (--id td-gen-N) that selects its own \
root and boots its own system (gnu.system/gnu.load) and no other's; the managed \
block defaults to the newest generation and carries the manual-rollback hook~a; \
the user preamble is preserved; pruned generations leave no boot dir, no root \
content, and no menu entry.~%"
              (if (null? images)
                  ", format-checked"
                  ", value-checked against the artifact")
              (if mkfs? " as a LIVE labeled ext4 filesystem (superblock-verified) carrying its appended dm-verity hash tree (verity-superblock-verified, recorded roothash/hashoffset)" "")
              (if boot-label " and the boot-partition search line" ""))
      (exit 0))
    (begin
      (format #t "~a check(s) failed.~%" failures)
      (exit 1)))
