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
;; the already-warm base closure — with substitutes off (triage #1) a cold path
;; would force a slow from-source build, which is wrong for a cheap, fast rung.
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
   (cons "host-name"              (td-config #:host-name "other-host"))
   (cons "timezone"              (td-config #:timezone "Europe/Paris"))
   (cons "locale"                (td-config #:locale "fr_FR.utf8"))
   (cons "root-fs-label"         (td-config #:root-fs-label "other-root"))
   (cons "ssh-port"              (td-config #:ssh-port 2222))
   (cons "ssh-password-auth?"    (td-config #:ssh-password-auth? #t))
   (cons "ssh-challenge-response?" (td-config #:ssh-challenge-response? #t))
   (cons "manifest"             (td-config #:manifest (cons hello %base-packages)))))

;; (B) Invalid values — each must be rejected at construction (the constructor
;; raises). Covers type and range/membership for every field, incl. root-mount.
(define invalid-rejections
  (list
   (cons "host-name empty"               (lambda () (td-config #:host-name "")))
   (cons "host-name non-string"          (lambda () (td-config #:host-name 42)))
   (cons "timezone empty"                (lambda () (td-config #:timezone "")))
   (cons "locale empty"                  (lambda () (td-config #:locale "")))
   (cons "bootloader-target relative"    (lambda () (td-config #:bootloader-target "dev/sda")))
   (cons "bootloader-target non-string"  (lambda () (td-config #:bootloader-target 42)))
   (cons "root-fs-label empty"           (lambda () (td-config #:root-fs-label "")))
   (cons "root-mount relative"           (lambda () (td-config #:root-mount "mnt")))
   (cons "root-fs-type unknown"          (lambda () (td-config #:root-fs-type "zfs")))
   (cons "ssh-port zero"                 (lambda () (td-config #:ssh-port 0)))
   (cons "ssh-port too-big"              (lambda () (td-config #:ssh-port 70000)))
   (cons "ssh-port non-integer"          (lambda () (td-config #:ssh-port "22")))
   (cons "ssh-password-auth? non-bool"   (lambda () (td-config #:ssh-password-auth? "yes")))
   (cons "ssh-challenge-response? non-bool" (lambda () (td-config #:ssh-challenge-response? 1)))
   (cons "manifest non-list"             (lambda () (td-config #:manifest 42)))
   (cons "manifest non-packages"         (lambda () (td-config #:manifest (list 1 2))))))

;; (C) Structural wiring — for the three fields that drv-divergence (A) cannot
;; probe. Each row is (NAME PERTURBED-CONFIG PREDICATE): we lower the perturbed
;; config to an operating-system RECORD (no derivation, no fs-tools, no build)
;; and the predicate asserts the perturbed value actually reached the compiled
;; field. If the compiler hard-coded or ignored the field, the predicate goes
;; red — exactly the wiring guarantee (A) gives the other fields.
(define (root-file-system os)
  ;; The td root fs is the only one whose device is a <file-system-label>
  ;; (%base-file-systems use paths / "none"); robust under any perturbation.
  (find (lambda (fs) (file-system-label? (file-system-device fs)))
        (operating-system-file-systems os)))

(define structural-wiring
  (list
   (list "bootloader-target"
         (td-config #:bootloader-target "/dev/sdb")
         (lambda (os)
           (equal? (bootloader-configuration-targets
                    (operating-system-bootloader os))
                   (list "/dev/sdb"))))
   (list "root-mount"
         (td-config #:root-mount "/altroot")
         (lambda (os)
           (and=> (root-file-system os)
                  (lambda (fs) (string=? (file-system-mount-point fs) "/altroot")))))
   (list "root-fs-type"
         (td-config #:root-fs-type "btrfs")
         (lambda (os)
           (and=> (root-file-system os)
                  (lambda (fs) (string=? (file-system-type fs) "btrfs")))))))

(define (raises? thunk)
  (catch #t (lambda () (thunk) #f) (lambda _ #t)))

(with-store store
  ;; Honest offline (triage #1): no substitutes for this store session.
  (set-build-options store #:use-substitutes? #f)

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
       (let* ((name (car row))
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
       (let* ((name (first row))
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
       (let* ((name (car row))
              (ok?  (raises? (cdr row))))
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
      (format #t "PASS: every field is wired into the lowered system (by \
drv-divergence or structural assertion), and every field rejects an invalid \
value at construction. (Note: string fields are type/shape-validated, not \
checked for semantic existence — e.g. an unknown-but-well-formed timezone or \
locale is accepted.)~%")
      (exit 0)))))
