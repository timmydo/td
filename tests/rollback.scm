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
;; THE STATE MODEL (DESIGN §2.6, M10.3 staged scope) is asserted across the same
;; two boots: on each boot the declared persistent path (/var/lib/ssh, the
;; default allowlist's precious entry) is BACKED BY td-state — the one writable
;; filesystem — not by that generation's root; a sentinel written under the
;; declared path in gen 2 survives into the gen-1 boot while an UNDECLARED
;; write does not follow the swap; and the SSH host key minted by gen 2's first
;; boot is byte-identical under gen 1 — rollback swaps the OS, never the
;; machine identity.
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

;; §2.6 state-model sentinels: one under a DECLARED persistent path (must
;; survive the swap via td-state), one under an UNDECLARED path (must not — it
;; stays inside gen-2's root).
(define %declared-sentinel-file "/var/lib/ssh/td-declared-sentinel")
(define %declared-sentinel "TD-DECLARED-99517")
(define %undeclared-sentinel-file "/var/td-undeclared-sentinel")

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

            ;; §2.6 state probe, one round trip: wait for the declared path's
            ;; bind mount (shepherd mounts it before user-processes, but the
            ;; marionette REPL can come up earlier), then report whether the
            ;; path is backed by td-state (not the generation root), the host
            ;; key's public half, and the sentinels' state.
            (define (guest-state marionette)
              (marionette-eval
               `(begin
                  (use-modules (ice-9 rdelim))
                  (define (dev-of f) (stat:dev (stat f)))
                  (let loop ((i 0))
                    (when (and (< i 30)
                               (or (not (file-exists? "/var/lib/ssh"))
                                   (= (dev-of "/var/lib/ssh") (dev-of "/"))))
                      (sleep 1) (loop (+ i 1))))
                  (let ((slurp (lambda (f)
                                 (and (file-exists? f)
                                      (call-with-input-file f read-string)))))
                    (list (and (file-exists? "/var/lib/ssh")
                               (not (= (dev-of "/var/lib/ssh") (dev-of "/")))
                               (= (dev-of "/var/lib/ssh")
                                  (dev-of ,#$%td-state-mount-point)))
                          (slurp ,(string-append #$%td-ssh-host-key ".pub"))
                          (slurp ,#$%declared-sentinel-file)
                          (file-exists? ,#$%undeclared-sentinel-file))))
               marionette))

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

            ;; The gen-2 host key, carried across the reboot so boot 2 can
            ;; prove machine identity survived the OS swap.
            (define host-key-from-gen-2 #f)

            ;; ---------- boot 1: the GRUB default = newest generation (2) ----
            (mkdir "m1")
            (let* ((m1 (make-marionette vm-command #:socket-directory "m1"))
                   (state-1 (begin
                              (assert-generation! m1 2 #$(gen-label 2)
                                                  #$gen2-os #$(gen-label 1))
                              (guest-state m1)))
                   (host-key-1 (cadr state-1)))
              (set! host-key-from-gen-2 host-key-1)
              ;; §2.6 on gen 2: the declared path is BACKED BY td-state (not
              ;; this generation's root), and first boot minted the machine's
              ;; SSH host key there (activation, via the backing path).
              (test-equal "gen 2: declared path /var/lib/ssh is backed by td-state"
                #t (car state-1))
              (test-assert "gen 2: first boot minted the SSH host key on the precious tier"
                (and (string? host-key-1)
                     (string-contains host-key-1 "ssh-ed25519")))

              ;; Write the two state-model sentinels: declared (must survive
              ;; the swap) and undeclared (must stay behind in gen-2's root).
              (test-assert "gen 2: declared + undeclared sentinels written"
                (marionette-eval
                 '(begin
                    (call-with-output-file #$%declared-sentinel-file
                      (lambda (p) (display #$%declared-sentinel p)))
                    (call-with-output-file #$%undeclared-sentinel-file
                      (lambda (p) (display "should-not-follow-the-swap" p)))
                    (and (file-exists? #$%declared-sentinel-file)
                         (file-exists? #$%undeclared-sentinel-file)))
                 m1))

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

              ;; §2.6 on gen 1 — both directions of declared persistence, plus
              ;; machine identity:
              ;;   * the declared-path sentinel followed the swap (td-state);
              ;;   * the undeclared write did NOT (it stayed in gen-2's root);
              ;;   * the SSH host key is byte-identical — rollback swapped the
              ;;     OS, never the machine.
              (let ((state-2 (guest-state m2)))
                (test-equal "gen 1: declared path /var/lib/ssh is backed by td-state"
                  #t (car state-2))
                (test-equal "gen 1: the DECLARED-path sentinel survived the swap"
                  #$%declared-sentinel (caddr state-2))
                (test-equal "gen 1: the UNDECLARED write did not follow the swap"
                  #f (cadddr state-2))
                (test-assert "gen 1: SSH host key identical — rollback never changes machine identity"
                  (and (string? host-key-from-gen-2)
                       (equal? (cadr state-2) host-key-from-gen-2))))

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
and menu entry) persisted across the reboot; and assert the §2.6 state model \
— declared paths are backed by td-state and survive the swap, an undeclared \
write does not, and the SSH host key (machine identity) is unchanged by the \
rollback.")
   (value (run-td-rollback-test))))
