;; tests/typed-coverage.scm — M4 typed front-end coverage (triage #4).
;;
;; tests/typed-diff.scm proves the typed default CONVERGES to the oracle and that
;; ONE perturbation (ssh-port) diverges. That left a hole: every OTHER field could
;; be silently ignored by the compiler, or have its validation removed, and the
;; suite would stay green. This rung closes that hole with two table-driven
;; sweeps over EVERY field:
;;
;;   (A) WIRING — for each field, a VALID non-default value must lower to a
;;       DIFFERENT system derivation than the oracle. If the compiler ignored a
;;       field (e.g. hard-coded the host-name), perturbing it would NOT change
;;       the drv → that row goes red. So this proves each field actually reaches
;;       the lowered operating-system.
;;   (B) VALIDATION — for each field, an INVALID value must be REJECTED by the
;;       `td-config` smart constructor (it raises). If validation for a field
;;       were dropped, its row would construct successfully → red. This automates
;;       the "schema with teeth" guarantee that was previously only checked by
;;       hand.
;;
;; Both sweeps are inherently self-discriminating per-field (the M3 false-green
;; lesson): a regression in any single field reddens exactly its row. No image is
;; built — these are derivation-level lowerings and pure constructor calls.
;;
;; SCHEMA-DERIVED DENOMINATOR (triage — coverage cannot silently fall behind the
;; schema). The set of fields under test is NOT a hand-maintained count: every
;; row is tagged with the record field SYMBOL it exercises, and a preflight
;; introspects the canonical field list straight from the <td-config> record
;; (`record-type-fields`) and asserts that the wiring rows cover EXACTLY those
;; fields (each once — no omission, no extra, no duplicate) and that the
;; validation rows cover EXACTLY those fields (≥1 each). Add a twelfth record
;; field without a matching row and this rung goes red before any sweep runs —
;; the "every field is wired" claim can no longer outrun the actual record.
;;
;; Run as a script so the process exit status is the result (`guix repl FILE`
;; honors `(exit)`; a STDIN-piped script would not — see typed-diff.scm).
(use-modules (guix store)
             (guix derivations)
             (guix monads)
             (gnu)
             (gnu system)
             (gnu bootloader)           ;bootloader-configuration-targets
             (gnu system file-systems)  ;file-system-* accessors
             (gnu packages base)        ;hello
             (srfi srfi-1)
             (ice-9 format)
             (system td)
             (system td-typed))

;; (A) Valid, non-default perturbations — one per field. Each must change the
;; lowered SYSTEM derivation, and (deliberately) must NOT pull anything outside
;; the already-warm base closure — with substitutes off (triage) a cold path
;; would force a slow from-source build, which is wrong for a cheap, fast rung.
;; Each row is (FIELD-SYMBOL . PERTURBED-CONFIG); FIELD-SYMBOL must be a real
;; <td-config> field (the preflight checks this against the record).
;;
;; Three fields are NOT probeable by drv-divergence and are instead covered by
;; the STRUCTURAL sweep (C) below — NOT silently dropped (the earlier gap):
;;   * bootloader-target — the install *device* is consumed at image/install
;;     time, not baked into `operating-system-derivation`; perturbing it does not
;;     change the system drv (verified), so it is not a valid wiring probe here.
;;   * root-fs-type — any non-ext4 type pulls its fs-tools (e.g. btrfs-progs),
;;     which are not in the warm closure; lowering it to a DERIVATION offline
;;     triggers a from-source build. (Lowering to a RECORD, as sweep C does, does
;;     not — so its wiring is checked there.)
;;   * root-mount — could change the drv, but it is grouped with the other two
;;     and checked structurally in (C) so all three root/bootloader fields are
;;     asserted the same way.
(define valid-perturbations
  (list
   (cons 'host-name              (td-config #:host-name "other-host"))
   (cons 'timezone              (td-config #:timezone "Europe/Paris"))
   (cons 'locale                (td-config #:locale "fr_FR.utf8"))
   (cons 'root-fs-label         (td-config #:root-fs-label "other-root"))
   (cons 'ssh-port              (td-config #:ssh-port 2222))
   (cons 'ssh-password-auth?    (td-config #:ssh-password-auth? #t))
   (cons 'ssh-challenge-response? (td-config #:ssh-challenge-response? #t))
   (cons 'manifest             (td-config #:manifest (cons hello %base-packages)))
   ;; M7: ship-guix? #f deletes guix-service-type, shrinking the system closure
   ;; (a subset — it pulls nothing cold, safe for this substitutes-off rung) and
   ;; so diverging the system drv from the oracle. Proves the field is wired.
   (cons 'ship-guix?           (td-config #:ship-guix? #f))))

;; (C) Structural wiring — for the three fields that drv-divergence (A) cannot
;; probe. Each row is (FIELD-SYMBOL PERTURBED-CONFIG PREDICATE): we lower the
;; perturbed config to an operating-system RECORD (no derivation, no fs-tools, no
;; build) and the predicate asserts the perturbed value actually reached the
;; compiled field. If the compiler hard-coded or ignored the field, the predicate
;; goes red — exactly the wiring guarantee (A) gives the other fields.
(define (root-file-system os)
  ;; The td root fs is the only one whose device is a <file-system-label>
  ;; (%base-file-systems use paths / "none"); robust under any perturbation.
  (find (lambda (fs) (file-system-label? (file-system-device fs)))
        (operating-system-file-systems os)))

(define structural-wiring
  (list
   (list 'bootloader-target
         (td-config #:bootloader-target "/dev/sdb")
         (lambda (os)
           (equal? (bootloader-configuration-targets
                    (operating-system-bootloader os))
                   (list "/dev/sdb"))))
   (list 'root-mount
         (td-config #:root-mount "/altroot")
         (lambda (os)
           (and=> (root-file-system os)
                  (lambda (fs) (string=? (file-system-mount-point fs) "/altroot")))))
   (list 'root-fs-type
         (td-config #:root-fs-type "btrfs")
         (lambda (os)
           (and=> (root-file-system os)
                  (lambda (fs) (string=? (file-system-type fs) "btrfs")))))))

;; (B) Invalid values — each must be rejected at construction (the constructor
;; raises). Covers type and range/membership for every field. Each row is
;; (FIELD-SYMBOL DESCRIPTION THUNK); a field may have several rows.
(define invalid-rejections
  (list
   (list 'host-name "host-name empty"               (lambda () (td-config #:host-name "")))
   (list 'host-name "host-name non-string"          (lambda () (td-config #:host-name 42)))
   (list 'timezone "timezone empty"                 (lambda () (td-config #:timezone "")))
   (list 'locale "locale empty"                     (lambda () (td-config #:locale "")))
   (list 'bootloader-target "bootloader-target relative"   (lambda () (td-config #:bootloader-target "dev/sda")))
   (list 'bootloader-target "bootloader-target non-string" (lambda () (td-config #:bootloader-target 42)))
   (list 'root-fs-label "root-fs-label empty"       (lambda () (td-config #:root-fs-label "")))
   (list 'root-mount "root-mount relative"          (lambda () (td-config #:root-mount "mnt")))
   (list 'root-fs-type "root-fs-type unknown"       (lambda () (td-config #:root-fs-type "zfs")))
   (list 'ssh-port "ssh-port zero"                  (lambda () (td-config #:ssh-port 0)))
   (list 'ssh-port "ssh-port too-big"               (lambda () (td-config #:ssh-port 70000)))
   (list 'ssh-port "ssh-port non-integer"           (lambda () (td-config #:ssh-port "22")))
   (list 'ssh-password-auth? "ssh-password-auth? non-bool"   (lambda () (td-config #:ssh-password-auth? "yes")))
   (list 'ssh-challenge-response? "ssh-challenge-response? non-bool" (lambda () (td-config #:ssh-challenge-response? 1)))
   (list 'manifest "manifest non-list"              (lambda () (td-config #:manifest 42)))
   (list 'manifest "manifest non-packages"          (lambda () (td-config #:manifest (list 1 2))))
   (list 'ship-guix? "ship-guix? non-bool"          (lambda () (td-config #:ship-guix? "yes")))))

(define (raises? thunk)
  (catch #t (lambda () (thunk) #f) (lambda _ #t)))

;;;
;;; Schema-coverage preflight (triage). The denominator is the actual record, not
;;; a hand-kept count: derive the canonical field list from <td-config> and assert
;;; the tables cover exactly it. A schema change with no matching row reddens here.
;;;

(define canonical-fields (record-type-fields <td-config>))
(define wiring-fields (append (map car valid-perturbations)
                              (map first structural-wiring)))
(define validation-fields (map first invalid-rejections))

(define (set- a b) (lset-difference eq? a b))

;; Elements that occur more than once in LST (each reported once). Computed by
;; occurrence count — NOT (set- lst (delete-duplicates lst)), which is always '()
;; because delete-duplicates retains every distinct element, so the difference
;; can never surface a repeat.
(define (duplicates lst)
  (delete-duplicates
   (filter (lambda (x) (> (count (lambda (y) (eq? x y)) lst) 1)) lst)))

(let* (;; wiring must cover each field EXACTLY once (no omission, no extra, no dup)
       (wiring-missing   (set- canonical-fields wiring-fields))
       (wiring-unknown   (set- wiring-fields canonical-fields))
       (wiring-dups      (duplicates wiring-fields))
       ;; validation must cover each field at least once (no omission, no unknown)
       (val-distinct     (delete-duplicates validation-fields))
       (val-missing      (set- canonical-fields val-distinct))
       (val-unknown      (set- val-distinct canonical-fields)))
  (format #t "~%== M4 typed coverage: schema preflight ==~%")
  (format #t "  canonical <td-config> fields (~a): ~a~%"
          (length canonical-fields) canonical-fields)
  (format #t "  wiring rows cover ~a field(s); validation rows cover ~a field(s)~%"
          (length (delete-duplicates wiring-fields)) (length val-distinct))
  (when (or (pair? wiring-missing) (pair? wiring-unknown) (pair? wiring-dups))
    (format #t "FAIL: wiring coverage does not match the record schema.~%")
    (when (pair? wiring-missing)
      (format #t "  fields with NO wiring row (add one): ~a~%" wiring-missing))
    (when (pair? wiring-unknown)
      (format #t "  wiring rows naming a NON-field: ~a~%" wiring-unknown))
    (when (pair? wiring-dups)
      (format #t "  fields wired more than once: ~a~%" wiring-dups))
    (exit 1))
  (when (or (pair? val-missing) (pair? val-unknown))
    (format #t "FAIL: validation coverage does not match the record schema.~%")
    (when (pair? val-missing)
      (format #t "  fields with NO validation row (add one): ~a~%" val-missing))
    (when (pair? val-unknown)
      (format #t "  validation rows naming a NON-field: ~a~%" val-unknown))
    (exit 1))
  (format #t "  ok: every record field has a wiring row (exactly once) and a \
validation row.~%"))

(with-store store
  ;; Offline contract (triage): no substitutes and no remote offloading for this
  ;; store session (guix repl ignores GUIX_BUILD_OPTIONS). Guarantees no binary
  ;; substitutes and no remote builders; a cold fixed-output SOURCE fetch by the
  ;; shared daemon is still possible (the narrowed contract — see check.sh).
  (set-build-options store #:use-substitutes? #f #:offload? #f)

  (define (sys-drv os)
    (derivation-file-name
     (run-with-store store (operating-system-derivation os))))

  (define oracle (sys-drv td-system))

  (format #t "~%== M4 typed coverage: per-field wiring + validation ==~%")
  (format #t "  oracle system drv: ~a~%~%" oracle)

  ;; (A) wiring sweep
  (format #t "  (A) WIRING — a valid perturbation must diverge from the oracle:~%")
  (define wiring-failures
    (fold
     (lambda (row failures)
       (let* ((name (symbol->string (car row)))
              (drv  (sys-drv (td-config->operating-system (cdr row))))
              (ok?  (not (string=? drv oracle))))
         (format #t "      ~a ~a~%" (if ok? "ok  " "FAIL") name)
         (if ok? failures (cons name failures))))
     '() valid-perturbations))

  ;; (C) structural wiring sweep (the three drv-invisible fields)
  (format #t "~%  (C) STRUCTURAL WIRING — perturbed value must reach the \
compiled operating-system:~%")
  (define structural-failures
    (fold
     (lambda (row failures)
       (let* ((name (symbol->string (first row)))
              (os   (td-config->operating-system (second row)))
              (ok?  ((third row) os)))
         (format #t "      ~a ~a~%" (if ok? "ok  " "FAIL") name)
         (if ok? failures (cons name failures))))
     '() structural-wiring))

  ;; (B) validation sweep
  (format #t "~%  (B) VALIDATION — an invalid value must be rejected:~%")
  (define validation-failures
    (fold
     (lambda (row failures)
       (let* ((name (second row))
              (ok?  (raises? (third row))))
         (format #t "      ~a ~a~%" (if ok? "ok  " "FAIL") name)
         (if ok? failures (cons name failures))))
     '() invalid-rejections))

  (let ((wf (length wiring-failures))
        (sf (length structural-failures))
        (vf (length validation-failures))
        ;; total fields actually wiring-checked: drv-divergence (A) + structural (C)
        (wired-checked (+ (length valid-perturbations) (length structural-wiring))))
    (format #t "~%  wiring: ~a/~a reached the system (~a via drv-divergence, ~a \
structural)   validation: ~a/~a rejected~%"
            (- wired-checked wf sf) wired-checked
            (- (length valid-perturbations) wf)
            (- (length structural-wiring) sf)
            (- (length invalid-rejections) vf) (length invalid-rejections))
    (cond
     ((> wf 0)
      (format #t "FAIL: ~a field(s) did NOT change the system when perturbed \
(ignored by the compiler): ~a~%" wf wiring-failures)
      (exit 1))
     ((> sf 0)
      (format #t "FAIL: ~a field(s) did NOT reach the compiled operating-system \
(ignored by the compiler): ~a~%" sf structural-failures)
      (exit 1))
     ((> vf 0)
      (format #t "FAIL: ~a invalid value(s) were ACCEPTED (validation missing): \
~a~%" vf validation-failures)
      (exit 1))
     (else
      (format #t "PASS: every record field (per <td-config> introspection) is \
wired into the lowered system (by drv-divergence or structural assertion), and \
every field rejects an invalid value at construction. (Note: string fields are \
type/shape-validated, not checked for semantic existence — e.g. an \
unknown-but-well-formed timezone or locale is accepted.)~%")
      (exit 0)))))
