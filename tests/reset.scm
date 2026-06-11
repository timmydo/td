;; tests/reset.scm — loop-latency: per-test ephemerality of the CoW VM reset
;; (DESIGN §1.5, plan/loop-latency.md sub-task 2).
;;
;; The loop's per-test "fresh state" guarantee rests on copy-on-write boots: the
;; marionette rungs never write the store image itself — guest writes land in a
;; discardable overlay (`-snapshot` in boot-disk, volatile root in the VM tests).
;; Until now that guarantee was IMPLICIT in qemu flags; nothing would go red if a
;; speed change (this track's business) quietly made guest state leak across
;; boots. This rung makes it an assertion, with the explicit qcow2-overlay
;; mechanism §1.5 names.
;;
;; Three boots of a NON-volatile qcow2 of the same instrumented system the
;; boot-disk rung boots (see %persistent-instrumented-image below for why the
;; stock image cannot work here), each on an explicit CoW overlay backed by
;; the read-only store image:
;;
;;   boot 1, overlay A : guest writes dirt (/root/td-dirt), syncs, quits.
;;   boot 2, overlay A : NO reset — the dirt MUST still be there. This is the
;;                       committed negative control (the M3 lesson): it proves
;;                       guest writes genuinely persist when the overlay is
;;                       reused, so boot 3's cleanliness is a property of the
;;                       RESET, not of writes never landing.
;;   boot 3, overlay B : the reset under test — a fresh overlay over the same
;;                       backing image MUST show pristine state (dirt gone).
;;
;; Verified-red (recorded in plan/loop-latency.md): point boot 3 at overlay A
;; (skip the reset) and the "dirt is gone" assertion fails; give boot 2 a fresh
;; overlay and the "dirt persists" control fails.
;;
;; The backing store image is opened read-only by qemu (CoW), so /gnu/store
;; immutability is never at risk; all overlays live in the build directory and
;; die with it. This is TEST-harness ephemerality — distinct from M10's
;; legitimate guest-persistent generations, which live inside one test.
(define-module (tests reset)
  #:use-module (gnu tests)
  #:use-module (gnu image)
  #:use-module (gnu system image)
  #:use-module (gnu packages virtualization)
  #:use-module ((gnu build marionette) #:select (qemu-command))
  #:use-module (guix gexp)
  #:use-module (tests boot)
  #:export (%test-td-reset))

;; A NON-volatile qcow2 of the same marionette-instrumented td-system the
;; boot-disk rung boots. Discovered red-first: the stock qcow2 image type
;; inherits <image>'s volatile-root? #t default, so EVERY guest write lands on
;; an in-RAM overlay and the persistence control below can never see it — the
;; first run of this rung failed exactly there (plan/loop-latency.md). The
;; mechanism under test is the QEMU-LEVEL CoW overlay reset (§1.5), so the
;; guest must be the strictest case: one that genuinely persists its writes,
;; isolated ONLY by the overlay. Costs one extra image derivation; everything
;; below it is shared with the other rungs' closures.
(define %persistent-instrumented-image
  (system-image
   (image
    (inherit ((image-type-constructor qcow2-image-type)
              %instrumented-disk-os))
    (volatile-root? #f))))

(define (run-td-reset-test)
  (define test
    (with-imported-modules '((gnu build marionette)
                             (guix build utils))
      #~(begin
          (use-modules (gnu build marionette)
                       (guix build utils)
                       (srfi srfi-64))

          (define qemu
            (string-append #$qemu-minimal "/bin/" #$(qemu-command)))
          (define qemu-img
            (string-append #$qemu-minimal "/bin/qemu-img"))
          (define image #$%persistent-instrumented-image)

          ;; A fresh CoW overlay backed by the (read-only) store image. THE
          ;; reset under test is exactly this: replace the overlay, lose the
          ;; writes. Deleting first makes re-creation an honest reset even if
          ;; the name is reused.
          (define (make-overlay! file)
            (when (file-exists? file)
              (delete-file file))
            (unless (zero? (system* qemu-img "create" "-q" "-f" "qcow2"
                                    "-b" image "-F" "qcow2" file))
              (error "qemu-img create failed" file)))

          ;; Boot the overlay; like boot-disk, no -kernel/-initrd — the full
          ;; firmware->GRUB->disk path. One socket dir per boot so sequential
          ;; marionettes never collide on their unix sockets. Generous accept
          ;; timeout: this rung may run beside other VM rungs under load.
          (define (boot overlay socket-dir)
            (mkdir-p socket-dir)
            (make-marionette
             `(,qemu
               ,@(if (file-exists? "/dev/kvm") '("-enable-kvm") '())
               "-no-reboot"
               "-m" "512"
               "-drive" ,(string-append "file=" overlay
                                        ",if=virtio,format=qcow2"))
             #:socket-directory socket-dir
             #:timeout 120))

          ;; Terminate qemu NOW (documented: "quit" returns no output) and reap
          ;; it, so the next boot may safely reuse the overlay file.
          (define (quit! marionette)
            (marionette-control "quit" marionette)
            (waitpid (marionette-pid marionette)))

          (test-runner-current (system-test-runner #$output))
          (test-begin "td-reset")

          (make-overlay! "/tmp/a.qcow2")
          (let ((m (boot "/tmp/a.qcow2" "/tmp/m1")))
            (test-assert "boot 1 (overlay A): guest dirties state and syncs it"
              (marionette-eval
               '(begin
                  (call-with-output-file "/root/td-dirt"
                    (lambda (port)
                      (display "td-dirt" port)))
                  (sync)
                  (file-exists? "/root/td-dirt"))
               m))
            (quit! m))

          ;; Negative control: WITHOUT a reset the dirt survives the reboot —
          ;; proves writes persist, so the reset (not write-loss) explains
          ;; boot 3's cleanliness.
          (let ((m (boot "/tmp/a.qcow2" "/tmp/m2")))
            (test-assert "boot 2 (overlay A reused, NO reset): dirt persists"
              (marionette-eval '(file-exists? "/root/td-dirt") m))
            (quit! m))

          ;; The reset under test: fresh overlay, same backing image, pristine
          ;; state.
          (make-overlay! "/tmp/b.qcow2")
          (let ((m (boot "/tmp/b.qcow2" "/tmp/m3")))
            (test-assert "boot 3 (fresh overlay B = the reset): dirt is gone"
              (not (marionette-eval '(file-exists? "/root/td-dirt") m)))
            (quit! m))

          (test-end)
          (exit (zero? (test-runner-fail-count (test-runner-current)))))))

  (gexp->derivation "td-reset-test" test))

(define %test-td-reset
  (system-test
   (name "td-reset")
   (description
    "Per-test ephemerality of the CoW VM reset (DESIGN §1.5): guest-dirtied \
state survives a reboot on the SAME qcow2 overlay (negative control) and is \
gone on a fresh overlay over the same backing image (the reset). Locks in the \
fresh-state-per-test guarantee that loop-latency work must preserve.")
   (value (run-td-reset-test))))
