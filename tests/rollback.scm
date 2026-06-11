;; tests/rollback.scm — M10.3: the manual-rollback acceptance test (DESIGN §7.1).
;;
;; From a disk carrying TWO placed generations (built by the guix-free placer,
;; assembled by system/td-disk.scm), ONE persistent qcow2 overlay is booted
;; TWICE through firmware -> GRUB:
;;
;;   boot 1 — GRUB's default is the newest entry (td-gen-2). Assert the booted
;;     identity (M11 shape): /proc/cmdline carries gen-2's RECORDED verity
;;     root hash (td.roothash=, the placer's cryptographic root selection —
;;     and NOT gen-1's; and NO root= at all), /gnu/store IS the dm-verity
;;     device /dev/mapper/td-root whose only backing slave is the partition
;;     labeled td-root-gen-2 (the slot binding, now UNDER the verifying
;;     layer), "/" is a tmpfs (the assembled root, §2.6), an undeclared
;;     write into the sealed store fails closed (EROFS — the §2.6
;;     enforcement stage), and /run/current-system is gen-2's system path
;;     (the menu's gnu.system wiring, not just the kernel cmdline).
;;     Then perform the MANUAL ROLLBACK ACT: mount the boot partition, write
;;     `set default=td-gen-1` into /td/default.cfg (the hook the placer's
;;     managed block sources), drop a persistence sentinel next to it, unmount,
;;     and reboot cleanly (shepherd's reboot; QEMU runs -no-reboot, so it exits).
;;
;;   boot 2 — SAME overlay. GRUB sources default.cfg and boots td-gen-1. Assert
;;     gen-1's identity the same ways (gen-2's root hash NOT on the cmdline),
;;     then assert the placed state PERSISTED across the reboot: the sentinel
;;     survived, default.cfg still holds the selection, gen-2's placed
;;     kernel/initrd are still on the boot partition, and the menu still lists
;;     BOTH generations (rolling back never destroys the newer generation —
;;     you can roll forward again).
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
  #:use-module (gnu packages linux)        ;e2fsprogs — debugfs (boot 3 target)
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
  (mlet %store-monad ((tree (td-rollback-tree))
                      (disk (td-rollback-disk-value)))
    (define gen1-os (gen-os 1))
    (define gen2-os (gen-os 2))

    (define test
      (with-imported-modules '((gnu build marionette))
        #~(begin
            (use-modules (gnu build marionette)
                         (srfi srfi-1)
                         (srfi srfi-13)
                         (srfi srfi-64)
                         (ice-9 format)
                         (ice-9 popen)
                         (ice-9 rdelim)
                         (rnrs bytevectors)
                         (rnrs io ports))

            (define qemu-img
              (string-append #$qemu-minimal "/bin/qemu-img"))

            ;; M11: each generation's EXPECTED verity root hash — read from
            ;; the placed tree's records (the same files the placer's menu
            ;; passes to the kernel), so the assertion pins the cmdline to
            ;; the artifact, not to a re-computation that could drift.
            (define (recorded-roothash n)
              (call-with-input-file
                  (string-append #$tree "/boot/td/gen-"
                                 (number->string n) "/verity-roothash")
                read-line))
            (define gen1-roothash (recorded-roothash 1))
            (define gen2-roothash (recorded-roothash 2))

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

            ;; M11 identity probe, one round trip. The facts, per generation:
            ;;   * kernel cmdline (carries td.roothash=/td.hashoffset= — the
            ;;     placer's cryptographic root selection — and NO root=);
            ;;   * /run/current-system (the menu's gnu.system wiring);
            ;;   * /gnu/store IS the dm-verity device /dev/mapper/td-root
            ;;     (st_dev of the store == st_rdev of the mapper node);
            ;;   * that dm device's ONLY backing slave is the partition
            ;;     LABELED with this generation's label (the slot binding —
            ;;     strictly stronger than the old mounted-root-by-label
            ;;     check: the label now sits UNDER the verifying layer);
            ;;   * "/" is a tmpfs (the assembled root, §2.6);
            ;;   * the store's ext4 SUPERBLOCK mount is itself read-only on
            ;;     the mapper device, it canNOT be remounted read-write, and
            ;;     an undeclared write fails closed (EROFS). Three layers
            ;;     deliberately probed PAST the %immutable-store ro BIND
            ;;     that %base-file-systems puts on top: that bind is
            ;;     software convention (guix-daemon remounts through it —
            ;;     run-with-writable-store), and a naive mkdir probe passes
            ;;     EVEN ON AN UNSEALED SYSTEM because of it (caught by
            ;;     variant H going false-green). So the probe first remounts
            ;;     the top bind rw (allowed — per-mountpoint flag), then
            ;;     attempts the SUPERBLOCK rw remount — which the kernel
            ;;     must refuse (the dm-verity device is read-only) — and
            ;;     only then probes the write.
            (define (guest-identity marionette label)
              (marionette-eval
               `(begin
                  (use-modules (ice-9 rdelim) (ice-9 ftw) (srfi srfi-1))
                  (define (firstline f)
                    (and (file-exists? f)
                         (call-with-input-file f read-line)))
                  (let* ((cmdline (call-with-input-file "/proc/cmdline"
                                    read-string))
                         (current (readlink "/run/current-system"))
                         (mapper  "/dev/mapper/td-root")
                         (store-on-verity?
                          (let loop ((i 0))
                            (cond ((file-exists? mapper)
                                   (= (stat:dev (stat "/gnu/store"))
                                      (stat:rdev (stat mapper))))
                                  ((< i 30) (sleep 1) (loop (+ i 1)))
                                  (else 'no-mapper-node))))
                         (dm-block
                          (find (lambda (b)
                                  (equal? (firstline
                                           (string-append "/sys/block/" b
                                                          "/dm/name"))
                                          "td-root"))
                                (or (scandir "/sys/block") '())))
                         (slaves
                          (and dm-block
                               (delete "." (delete ".."
                                 (or (scandir (string-append "/sys/block/"
                                                             dm-block
                                                             "/slaves"))
                                     '())))))
                         (labeled
                          (let ((dev (string-append "/dev/disk/by-label/"
                                                    ,label)))
                            (let loop ((i 0))
                              (cond ((file-exists? dev)
                                     (basename (readlink dev)))
                                    ((< i 30) (sleep 1) (loop (+ i 1)))
                                    (else 'no-by-label-node)))))
                         (slave-is-labeled? (equal? slaves (list labeled)))
                         (mounts-lines
                          (call-with-input-file "/proc/self/mounts"
                            (lambda (p)
                              (let loop ((acc '()))
                                (let ((line (read-line p)))
                                  (if (eof-object? line)
                                      (reverse acc)
                                      (loop (cons (string-split line #\space)
                                                  acc))))))))
                         (root-tmpfs?
                          (->bool (find (lambda (f)
                                          (and (>= (length f) 3)
                                               (string=? (list-ref f 1) "/")
                                               (string=? (list-ref f 2)
                                                         "tmpfs")))
                                        mounts-lines)))
                         ;; EVERY ext4 mount entry at /gnu/store must be ro
                         ;; (snapshot taken BEFORE the remount probes below).
                         ;; "Every" matters: the %immutable-store ro BIND
                         ;; shares the path and fstype, so a `find` of one ro
                         ;; entry would false-green on an unsealed system
                         ;; whose underlying sb mount says rw.
                         (store-ext4-ro?
                          (let ((entries
                                 (filter (lambda (f)
                                           (and (>= (length f) 4)
                                                (string=? (list-ref f 1)
                                                          "/gnu/store")
                                                (string=? (list-ref f 2)
                                                          "ext4")))
                                         mounts-lines)))
                            (and (pair? entries)
                                 (->bool
                                  (every (lambda (f)
                                           (member "ro"
                                                   (string-split
                                                    (list-ref f 3) #\,)))
                                         entries)))))
                         (mount-cmd
                          "/run/current-system/profile/bin/mount")
                         ;; Step past the convention layer: make the TOP
                         ;; bind's per-mountpoint flag rw (always allowed)...
                         (bind-remount-status
                          (system* mount-cmd "-o" "remount,bind,rw"
                                   "/gnu/store"))
                         ;; ...then attempt the SUPERBLOCK rw remount. On a
                         ;; sealed generation the kernel must REFUSE it (the
                         ;; dm-verity device is read-only); on an unsealed
                         ;; system it succeeds — the discriminator.
                         (sb-remount-rw-failed?
                          (not (zero? (system* mount-cmd "-o" "remount,rw"
                                               "/gnu/store"))))
                         (store-write-errno
                          (catch 'system-error
                            (lambda ()
                              (mkdir "/gnu/store/td-erofs-probe")
                              'write-succeeded!)
                            (lambda args (system-error-errno args)))))
                    (list cmdline current store-on-verity? slave-is-labeled?
                          root-tmpfs? store-write-errno
                          store-ext4-ro? sb-remount-rw-failed?)))
               marionette))

            (define (assert-generation! marionette n label roothash system
                                        foreign-roothash)
              (let* ((id      (guest-identity marionette label))
                     (cmdline (car id))
                     (current (cadr id))
                     (store-ok (list-ref id 2))
                     (slave-ok (list-ref id 3))
                     (tmpfs-ok (list-ref id 4))
                     (werrno   (list-ref id 5)))
                (test-assert
                    (format #f "boot ~a: cmdline passes generation ~a's verity root hash (and not the other's)"
                            n n)
                  (and (string-contains cmdline
                                        (string-append "td.roothash="
                                                       roothash " "))
                       (not (string-contains
                             cmdline
                             (string-append "td.roothash="
                                            foreign-roothash " ")))))
                (test-assert
                    (format #f "boot ~a: cmdline passes NO root= — / is the declared tmpfs" n)
                  (not (string-contains cmdline " root=")))
                (test-equal
                    (format #f "boot ~a: /gnu/store IS the dm-verity device /dev/mapper/td-root" n)
                  #t store-ok)
                (test-equal
                    (format #f "boot ~a: the verity data device is the partition labeled ~a" n label)
                  #t slave-ok)
                (test-equal
                    (format #f "boot ~a: / is a tmpfs — the root is assembled, not stored" n)
                  #t tmpfs-ok)
                (test-equal
                    (format #f "boot ~a: every store mount is read-only down to the ext4 superblock" n)
                  #t (list-ref id 6))
                (test-equal
                    (format #f "boot ~a: the store cannot be remounted read-write — sealing is kernel-enforced" n)
                  #t (list-ref id 7))
                (test-equal
                    (format #f "boot ~a: an undeclared write into the sealed store fails closed (EROFS)" n)
                  EROFS werrno)
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
                                                  gen2-roothash #$gen2-os
                                                  gen1-roothash)
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
              (assert-generation! m2 1 #$(gen-label 1) gen1-roothash
                                  #$gen1-os gen2-roothash)

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
                       (string-contains (list-ref persisted 4) "--id td-gen-2 "))))

              ;; M11 acceptance, the ACT for boot 3: roll FORWARD — select
              ;; gen-2 again through the same manual hook, so the next boot
              ;; targets the generation we are about to corrupt.
              (test-assert "roll-forward selected: default.cfg now boots td-gen-2 again"
                (marionette-eval
                 '(begin
                    (mkdir "/bootmnt2")
                    (and (zero? (system* "/run/current-system/profile/bin/mount"
                                         "/dev/vda1" "/bootmnt2"))
                         (begin
                           (call-with-output-file "/bootmnt2/td/default.cfg"
                             (lambda (p) (display "set default=td-gen-2\n" p)))
                           (zero? (system* "/run/current-system/profile/bin/umount"
                                           "/bootmnt2")))))
                 m2))
              (catch #t
                (lambda ()
                  (marionette-eval
                   '(begin
                      (sync)
                      (system* "/run/current-system/profile/sbin/reboot"))
                   m2))
                (lambda args #t))
              (catch #t
                (lambda () (waitpid (marionette-pid m2)))
                (lambda args #f)))

            ;; ---------- boot 3 (M11 §7.1 acceptance): a CORRUPTED root ----
            ;; ---------- fails CLOSED ------------------------------------
            ;; Corrupt ONE sector of gen-2's verity-protected DATA area in
            ;; the overlay, then boot again. The target is chosen so that
            ;; ONLY integrity verification can catch it: the first data
            ;; block of gen-2's gnu.load boot script (<system>/boot — the
            ;; very file the initrd loads right after mounting the store).
            ;; The ext4 superblock and the label stay INTACT, so partition
            ;; discovery and the mount succeed exactly as on a healthy
            ;; system — a label-based unsealed boot would run the corrupted
            ;; bytes without noticing. Here the first READ of the block must
            ;; fail closed: the kernel logs the dm-verity corruption
            ;; signature, the load fails, and the system never assembles
            ;; (shepherd never starts). No marionette — nothing comes up to
            ;; connect to; the serial log is the witness.

            ;; Partition N's byte offset, from the pristine disk's MBR
            ;; (entry base #x1be, 16 bytes each, start-LBA u32 LE at +8).
            (define (partition-start-bytes disk index)
              (let ((bv (call-with-input-file disk
                          (lambda (p)
                            (seek p (+ #x1be (* 16 index) 8) SEEK_SET)
                            (get-bytevector-n p 4))
                          #:binary #t)))
                (* 512 (bytevector-u32-ref bv 0 (endianness little)))))

            ;; vda1=boot, vda2=gen-1, vda3=gen-2 (index 2), vda4=td-state.
            (define gen2-start (partition-start-bytes #$disk 2))

            ;; The filesystem block (4096-byte units) holding the first data
            ;; block of gen-2's boot script, from debugfs over the PRISTINE
            ;; root.img in the placed tree. <system>/boot is a SYMLINK to a
            ;; separate -boot store item; resolve it on the build side (the
            ;; system closure is an input) — the image's fs root IS the
            ;; store content, so the resolved item lives at /<its basename>.
            (define gen2-boot-script-block
              (let* ((img  (string-append #$tree "/roots/td/gen-2/root.img"))
                     (path (string-append
                            "/" (basename
                                 (canonicalize-path
                                  (string-append #$gen2-os "/boot")))))
                     (port (open-input-pipe
                            (string-append
                             #$e2fsprogs "/sbin/debugfs -R 'blocks " path "' "
                             img " 2>/dev/null")))
                     (out  (get-string-all port)))
                (close-pipe port)
                (let ((tokens (filter (lambda (s) (not (string-null? s)))
                                      (string-split (string-trim-both out)
                                                    #\space))))
                  (and (pair? tokens) (string->number (car tokens))))))

            (define gen2-hashoffset
              (string->number
               (call-with-input-file
                   (string-append #$tree "/boot/td/gen-2/verity-hashoffset")
                 read-line)))

            ;; Guard: the target block is real and inside the verity DATA
            ;; area — otherwise this phase would "fail closed" vacuously.
            (test-assert "corruption target verified: gen-2's boot script block, inside the verity data area"
              (and gen2-boot-script-block
                   (> gen2-boot-script-block 0)
                   (< (* (+ gen2-boot-script-block 1) 4096) gen2-hashoffset)))

            (test-assert "one sector of gen-2's boot-script data block corrupted in the overlay"
              (zero? (system* (string-append #$qemu-minimal "/bin/qemu-io")
                              "-f" "qcow2" "-c"
                              (format #f "write -P 0xff ~a 512"
                                      (+ gen2-start
                                         (* gen2-boot-script-block 4096)))
                              "overlay.qcow2")))

            (define boot3-serial "serial3.log")
            (define boot3-command
              (append vm-command
                      (list "-display" "none"
                            "-serial" (string-append "file:" boot3-serial))))
            (define boot3-pid
              (let ((pid (primitive-fork)))
                (if (zero? pid)
                    (begin
                      (apply execlp (car boot3-command) boot3-command)
                      (primitive-exit 127))
                    pid)))

            (test-assert "boot 3 FAILS CLOSED: dm-verity reports the corruption and the system never assembles"
              (let loop ((i 0))
                (let ((txt (and (file-exists? boot3-serial)
                                (call-with-input-file boot3-serial
                                  get-string-all))))
                  (cond ((and txt
                              (string-contains txt "device-mapper: verity")
                              (string-contains txt "corrupted"))
                         ;; The corruption was detected; give the boot a
                         ;; moment more, then require the system never
                         ;; ASSEMBLED: shepherd (PID-1 of a successfully
                         ;; booted td system, visible on the serial console
                         ;; in boots 1 and 2) never started.
                         (sleep 5)
                         (let ((txt (call-with-input-file boot3-serial
                                      get-string-all)))
                           (not (string-contains txt "shepherd"))))
                        ((< i 180) (sleep 1) (loop (+ i 1)))
                        (else
                         (format #t "boot 3 serial after timeout:~%~a~%" txt)
                         #f)))))
            (kill boot3-pid SIGKILL)
            (catch #t
              (lambda () (waitpid boot3-pid))
              (lambda args #f))

            (test-end)
            (exit (zero? (test-runner-fail-count (test-runner-current)))))))

    (gexp->derivation "td-rollback-test" test)))

(define %test-td-rollback
  (system-test
   (name "td-rollback")
   (description
    "Boot a disk carrying two placed td generations through GRUB; assert the \
newest generation's identity (recorded verity root hash on the cmdline, \
store mounted read-only from the dm-verity device backed by the labeled \
partition, tmpfs root, EROFS on an undeclared store write, system path); \
select the older generation via the placer's manual-rollback hook; reboot \
the SAME disk overlay and assert the older generation's identity; assert \
the placed state (sentinel, selection, the newer generation's files and \
menu entry) persisted across the reboot; and assert the §2.6 state model — \
declared paths are backed by td-state and survive the swap, an undeclared \
write does not, and the SSH host key (machine identity) is unchanged by the \
rollback.")
   (value (run-td-rollback-test))))
