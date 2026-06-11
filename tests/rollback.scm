;; tests/rollback.scm — M10.3: the manual-rollback acceptance test (DESIGN §7.1).
;;
;; From a disk carrying TWO placed generations (built by the guix-free placer,
;; assembled by system/td-disk.scm), ONE persistent qcow2 overlay is booted
;; TWICE through firmware -> GRUB:
;;
;;   boot 1 — GRUB's default is the newest entry (td-gen-2). Assert the booted
;;     identity THREE independent ways: /proc/cmdline carries gen-2's
;;     root=<label>, the mounted root device IS the filesystem labeled
;;     td-root-gen-2 (st_dev of / == st_rdev of /dev/disk/by-label/...), and
;;     /run/current-system is gen-2's system path (set from the menu's
;;     gnu.system= — i.e. the placer's wiring, not just the kernel cmdline).
;;     Then perform the MANUAL ROLLBACK ACT: mount the boot partition, write
;;     `set default=td-gen-1` into /td/default.cfg (the hook the placer's
;;     managed block sources), drop a persistence sentinel next to it, unmount,
;;     and reboot cleanly (shepherd's reboot; QEMU runs -no-reboot, so it exits).
;;
;;   boot 2 — SAME overlay. GRUB sources default.cfg and boots td-gen-1. Assert
;;     gen-1's identity the same three ways (and that gen-2's root label is NOT
;;     on the cmdline), then assert the placed state PERSISTED across the
;;     reboot: the sentinel survived, default.cfg still holds the selection,
;;     gen-2's placed kernel/initrd are still on the boot partition, and the
;;     menu still lists BOTH generations (rolling back never destroys the newer
;;     generation — you can roll forward again).
;;
;; The guest OSes inside the generation bundles are instrumented with the
;; marionette backdoor via td-generation-image's #:transform-os — the same
;; overlay style as the disk-boot test; the SHIPPED bundles stay untouched.
(define-module (tests rollback)
  #:use-module (gnu tests)
  #:use-module (gnu packages virtualization)
  #:use-module ((gnu build marionette) #:select (qemu-command))
  #:use-module (guix gexp)
  #:use-module (guix monads)
  #:use-module (guix store)
  #:use-module (system td-typed)
  #:use-module (system td-place)
  #:use-module (system td-disk)
  #:export (td-rollback-tree
            td-rollback-disk-value
            %test-td-rollback))

;; The marionette instrumentation applied to each generation's OS — defined
;; ONCE and used both to build the bundles and to compute the expected
;; per-generation system paths, so they cannot drift apart.
(define (instrument os)
  (marionette-operating-system os #:imported-modules '((gnu services herd))))

(define %rollback-gens '(1 2))

(define (gen-os n)
  (instrument (td-config->operating-system (td-config #:generation n))))

(define (gen-label n)
  (td-config-effective-root-label (td-config #:generation n)))

;; The placed tree the disk is assembled from: live root filesystems (--mkfs),
;; the boot-partition search label, and a serial console so the guest's boot log
;; lands in the build output.
(define (td-rollback-tree)
  (td-placed-tree #:gens %rollback-gens
                  #:keep 10
                  #:mkfs? #t
                  #:boot-label %td-boot-label
                  #:extra-kernel-args "console=ttyS0"
                  #:transform-os instrument))

(define (td-rollback-disk-value)
  (mlet %store-monad ((tree (td-rollback-tree)))
    (td-rollback-disk tree #:gens %rollback-gens)))

(define %sentinel "TD-PERSIST-99517")

(define (run-td-rollback-test)
  (mlet %store-monad ((disk (td-rollback-disk-value)))
    (define gen1-os (gen-os 1))
    (define gen2-os (gen-os 2))

    (define test
      (with-imported-modules '((gnu build marionette))
        #~(begin
            (use-modules (gnu build marionette)
                         (srfi srfi-1)
                         (srfi srfi-13)
                         (srfi srfi-64)
                         (ice-9 format))

            (define qemu-img
              (string-append #$qemu-minimal "/bin/qemu-img"))

            ;; ONE persistent overlay for the whole test: guest writes survive
            ;; the in-test reboot (the acceptance clause), while the store disk
            ;; stays pristine (test isolation: the overlay dies with the build).
            (unless (zero? (system* qemu-img "create" "-f" "qcow2"
                                    "-b" #$disk "-F" "raw" "overlay.qcow2"))
              (error "could not create the persistent qcow2 overlay"))

            ;; No -kernel/-initrd and no -snapshot: every boot goes firmware ->
            ;; GRUB -> menu, and every write lands in the one overlay.
            (define vm-command
              `(,(string-append #$qemu-minimal "/bin/" #$(qemu-command))
                ,@(if (file-exists? "/dev/kvm") '("-enable-kvm") '())
                "-no-reboot"
                "-m" "1024"
                "-drive" "file=overlay.qcow2,if=virtio,format=qcow2"))

            (define (guest-identity marionette label)
              ;; Three independent identity facts, fetched in one round trip:
              ;; kernel cmdline, /run/current-system target, and whether the
              ;; mounted root IS the filesystem carrying this generation's
              ;; LABEL (st_dev of / vs st_rdev of the by-label device node,
              ;; which udev may take a moment to create).
              (marionette-eval
               `(begin
                  (use-modules (ice-9 rdelim))
                  (let* ((cmdline (call-with-input-file "/proc/cmdline"
                                    read-string))
                         (current (readlink "/run/current-system"))
                         (dev     (string-append "/dev/disk/by-label/" ,label))
                         (root-is-label?
                          (let loop ((i 0))
                            (cond ((file-exists? dev)
                                   (= (stat:dev (stat "/"))
                                      (stat:rdev (stat dev))))
                                  ((< i 30) (sleep 1) (loop (+ i 1)))
                                  (else 'no-by-label-node)))))
                    (list cmdline current root-is-label?)))
               marionette))

            (define (assert-generation! marionette n label system foreign-label)
              (let* ((id      (guest-identity marionette label))
                     (cmdline (car id))
                     (current (cadr id))
                     (root-ok (caddr id)))
                (test-assert
                    (format #f "boot ~a: cmdline selects ~a (and not ~a)"
                            n label foreign-label)
                  ;; Bare-label spec: Guix's initrd parses the whole root=
                  ;; value as the label (no dracut-style LABEL= prefix).
                  (and (string-contains cmdline
                                        (string-append "root=" label " "))
                       (not (string-contains
                             cmdline
                             (string-append "root=" foreign-label " ")))))
                (test-equal
                    (format #f "boot ~a: / IS the filesystem labeled ~a" n label)
                  #t root-ok)
                (test-equal
                    (format #f "boot ~a: /run/current-system is generation ~a's system" n n)
                  system current)))

            (test-runner-current (system-test-runner #$output))
            (test-begin "td-rollback")

            ;; Permanent structural guard (cf. the disk-boot rung): the boot
            ;; must go through the DISK's firmware->GRUB path with PERSISTENT
            ;; writes — no direct-kernel, no -snapshot.
            (test-assert "boots the placed disk via firmware->GRUB, persistently"
              (and (not (member "-kernel" vm-command))
                   (not (member "-initrd" vm-command))
                   (not (member "-snapshot" vm-command))
                   (any (lambda (a)
                          (and (string? a)
                               (string-contains a "overlay.qcow2")))
                        vm-command)))

            ;; ---------- boot 1: the GRUB default = newest generation (2) ----
            (mkdir "m1")
            (let ((m1 (make-marionette vm-command #:socket-directory "m1")))
              (assert-generation! m1 2 #$(gen-label 2) #$gen2-os #$(gen-label 1))

              ;; The manual rollback ACT + a persistence sentinel, written to
              ;; the boot partition through the running gen-2 system.
              (test-assert "rollback selected: default.cfg + sentinel written to the boot partition"
                (marionette-eval
                 '(begin
                    (mkdir "/bootmnt")
                    (and (zero? (system* "/run/current-system/profile/bin/mount"
                                         "/dev/vda1" "/bootmnt"))
                         (begin
                           (call-with-output-file "/bootmnt/td/default.cfg"
                             (lambda (p) (display "set default=td-gen-1\n" p)))
                           (call-with-output-file "/bootmnt/td/persist-sentinel"
                             (lambda (p) (display #$%sentinel p)))
                           (zero? (system* "/run/current-system/profile/bin/umount"
                                           "/bootmnt")))))
                 m1))

              ;; Clean reboot: shepherd stops services and unmounts; QEMU exits
              ;; (-no-reboot). The eval's connection may die mid-flight — that
              ;; is the expected outcome, not a failure; boot 2 is the proof.
              (catch #t
                (lambda ()
                  (marionette-eval
                   '(begin
                      (sync)
                      (system* "/run/current-system/profile/sbin/reboot"))
                   m1))
                (lambda args #t))
              (catch #t
                (lambda () (waitpid (marionette-pid m1)))
                (lambda args #f)))

            ;; ---------- boot 2: SAME overlay; GRUB sources default.cfg ------
            (mkdir "m2")
            (let ((m2 (make-marionette vm-command #:socket-directory "m2")))
              (assert-generation! m2 1 #$(gen-label 1) #$gen1-os #$(gen-label 2))

              ;; Placed state persisted across the reboot: the sentinel and the
              ;; selection survived, gen-2's placed files are intact, and the
              ;; menu still lists BOTH generations (roll-forward stays possible).
              (let ((persisted
                     (marionette-eval
                      '(begin
                         (use-modules (ice-9 rdelim))
                         (mkdir "/bootmnt")
                         (if (zero? (system* "/run/current-system/profile/bin/mount"
                                             "-o" "ro" "/dev/vda1" "/bootmnt"))
                             (let ((slurp (lambda (f)
                                            (and (file-exists? f)
                                                 (call-with-input-file f
                                                   read-string))))
                                   (exists (lambda (f) (file-exists? f))))
                               (let ((result
                                      (list (slurp "/bootmnt/td/persist-sentinel")
                                            (slurp "/bootmnt/td/default.cfg")
                                            (exists "/bootmnt/td/gen-2/bzImage")
                                            (exists "/bootmnt/td/gen-2/initrd.cpio.gz")
                                            (slurp "/bootmnt/grub/grub.cfg"))))
                                 (system* "/run/current-system/profile/bin/umount"
                                          "/bootmnt")
                                 result))
                             'mount-failed))
                      m2)))
                (test-assert "the sentinel written before the reboot persisted"
                  (and (list? persisted)
                       (equal? (car persisted) #$%sentinel)))
                (test-assert "the rollback selection (default.cfg) persisted"
                  (and (list? persisted)
                       (equal? (cadr persisted) "set default=td-gen-1\n")))
                (test-assert "generation 2's placed kernel+initrd survived the rollback"
                  (and (list? persisted)
                       (eq? (list-ref persisted 2) #t)
                       (eq? (list-ref persisted 3) #t)))
                (test-assert "the menu still lists BOTH generations"
                  (and (list? persisted)
                       (string? (list-ref persisted 4))
                       (string-contains (list-ref persisted 4) "--id td-gen-1 ")
                       (string-contains (list-ref persisted 4) "--id td-gen-2 ")))))

            (test-end)
            (exit (zero? (test-runner-fail-count (test-runner-current)))))))

    (gexp->derivation "td-rollback-test" test)))

(define %test-td-rollback
  (system-test
   (name "td-rollback")
   (description
    "Boot a disk carrying two placed td generations through GRUB; assert the \
newest generation's identity (root label, mounted root filesystem, system \
path); select the older generation via the placer's manual-rollback hook; \
reboot the SAME disk overlay and assert the older generation's identity; \
assert the placed state (sentinel, selection, the newer generation's files \
and menu entry) persisted across the reboot.")
   (value (run-td-rollback-test))))
