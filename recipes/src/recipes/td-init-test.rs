use crate::ladder::{post_bootstrap_path, POST_BOOTSTRAP_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// td-init-test: build-shape AND behavioural validation of the boot-glue multicall.
//
// Per repo policy that recipes test their output, this asserts the shipped
// td-init binary is the self-contained STATIC ELF its slot requires, re-proving
// with an independent readelf walk what the producer's `assert_static`
// fail-closes on:
//   1. ELF64 x86-64 *executable* (readelf: class ELF64, machine x86-64, type
//      EXEC) — EXEC (not DYN) is the non-PIE static shape,
//   2. NO PT_INTERP program header,
//   3. NO dynamic NEEDED entry — an EMPTY runtime closure.
//
// It then EXERCISES what can be exercised. Unlike td-util's diagnostics, most of
// these applets are IRREVERSIBLE OR PROCESS-REPLACING, so the behavioural legs
// are chosen for what a build sandbox can survive:
//
//   * reboot/poweroff/halt are NEVER invoked in a way that reaches reboot(2). A
//     sandbox with CAP_SYS_BOOT would reboot the BUILDER. Only the option-parse
//     rejection path is asserted — which is also the property that matters, since
//     a `poweroff -x` that fell through to the syscall would be a data-loss bug.
//   * hostname is never asked to SET one, for the same reason: sethostname(2)
//     would succeed and rename whatever UTS namespace the sandbox is in. Its
//     rejection paths are asserted, and printing is gated on /proc.
//   * switch_root is asserted through its FAIL-EARLY contract — a new root with
//     no usable init must be refused BEFORE any mount moves — which is exactly
//     the case a sandbox can run safely.
//   * cttyhack and init would replace or never leave the process, so cttyhack is
//     asserted through its usage path and init through `--dry-run`, the inittab
//     validator that parses a table and reports rejected lines through its exit
//     code.
//   * mount and umount are never asked to CHANGE the mount table: a build
//     sandbox is a mount namespace whose filesystems the rest of the derivation
//     is still standing on, and a `umount -a` here would take the store out from
//     under the check. Their argument parsing is asserted instead — which is the
//     property that matters, since every mount on this image is a fixed argument
//     list some script wrote once — plus `mount`'s read-only table listing where
//     /proc is available.
//
// The applets' live behaviour on a booted machine (PID 1 supervision, ctty
// acquisition, the real switch_root) belongs to the headless boot oracle, which
// gains it when system-x86-64 flips its /bin farm to td-init.
pub fn recipe() -> Recipe {
    let bin = "{in:td-init}/bin/td-init";
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let mut steps = Vec::new();

    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "h=$('{readelf}' -h '{bin}' 2>/dev/null) || {{ echo 'readelf -h failed on td-init' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || {{ echo 'td-init is not ELF64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || {{ echo 'td-init is not x86-64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -qE 'Type:[[:space:]]+EXEC([[:space:]]|$)' || {{ echo 'td-init is not a static ET_EXEC — a DYN/PIE (Type: DYN, whose parenthetical also says Executable) would need runtime relocation' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "lout=$('{readelf}' -l '{bin}' 2>/dev/null) || {{ echo 'readelf -l failed on td-init (cannot verify absence of PT_INTERP)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$lout\" | grep -qi 'INTERP'; then echo 'td-init carries a PT_INTERP program header — it is not static' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "dout=$('{readelf}' -d '{bin}' 2>/dev/null) || {{ echo 'readelf -d failed on td-init (cannot verify absence of dynamic NEEDED)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$dout\" | grep -qi 'NEEDED'; then echo 'td-init has a dynamic NEEDED entry — its runtime closure is not empty' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // Applet roster: the /bin symlink farm this multicall will back is generated
    // from `--list`, so a dropped or renamed applet must red here rather than
    // strand a dead /bin symlink — or, for `init`, an unbootable /sbin/init.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "l=$('{bin}' --list) || {{ echo 'td-init --list failed' >&2; exit 1; }}; \
                     for a in cttyhack halt hostname init losetup mknod mount poweroff reboot switch_root sync umount; do \
                         printf '%s\\n' \"$l\" | grep -q -x -F \"$a\" || {{ echo \"td-init does not serve applet '$a'\" >&2; exit 1; }}; \
                     done; \
                     n=$(printf '%s\\n' \"$l\" | wc -l); \
                     [ \"$n\" -eq 12 ] || {{ echo \"td-init serves $n applets, expected exactly 12 — update this check deliberately when adding one\" >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // The inittab fixtures `init --dry-run` parses. Written as files rather than
    // built with printf so the table reads as a table; dry-run never executes a
    // command, so these paths need not exist.
    steps.push(Step::WriteFile {
        path: "{root}/inittab.good".into(),
        content: "# td-init-test fixture\n\
                  \n\
                  ::sysinit:/bin/true\n\
                  ttyS0:2345:respawn:/bin/sh -c 'exit 0'\n\
                  ::once:/bin/echo one\n"
            .into(),
        exec: false,
    });
    steps.push(Step::WriteFile {
        path: "{root}/inittab.bad".into(),
        content: "::bogus:/bin/true\nnonsense\n".into(),
        exec: false,
    });

    // Dispatch through BOTH entry forms, plus the documented exit codes. The
    // irreversible applets are reached ONLY through their rejection paths — see
    // the header: nothing below may reach reboot(2) or sethostname(2).
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "'{bin}' no-such-applet >/dev/null 2>&1; \
                     [ $? -eq 2 ] || {{ echo 'td-init must exit 2 on an unknown applet (usage error)' >&2; exit 1; }}; \
                     '{bin}' cttyhack >/dev/null 2>&1; \
                     [ $? -eq 1 ] || {{ echo 'cttyhack with no PROG must exit 1' >&2; exit 1; }}; \
                     e=$('{bin}' reboot --not-an-option 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'reboot must reject an unknown option with exit 1 — falling through to reboot(2) would power the machine down on a typo' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'unrecognised argument' || {{ echo \"reboot rejected the option without saying so: '$e'\" >&2; exit 1; }}; \
                     d='{{root}}/argv0'; mkdir -p \"$d\"; \
                     ln -sf '{bin}' \"$d/switch_root\" || {{ echo 'could not build the argv[0] symlink' >&2; exit 1; }}; \
                     ln -sf '{bin}' \"$d/init\" || {{ echo 'could not build the argv[0] symlink' >&2; exit 1; }}; \
                     e=$(\"$d/switch_root\" 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'argv[0] dispatch: /sbin/switch_root -> td-init did not reach the applet' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'usage: switch_root' || {{ echo \"argv[0] dispatch produced '$e', expected the switch_root usage\" >&2; exit 1; }}; \
                     e=$('{bin}' losetup --not-an-option 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'losetup must reject a bad argument with exit 1 — reaching LOOP_SET_FD on a typo would bind a device nobody named' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'usage: losetup' || {{ echo \"losetup rejected the argument without saying so: '$e'\" >&2; exit 1; }}; \
                     e=$('{bin}' mknod {{root}}/mknod-probe c 7 0 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'mknod must refuse a non-block type with exit 1' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'BLOCK nodes only' || {{ echo \"mknod refused the type without saying so: '$e'\" >&2; exit 1; }}; \
                     e=$('{bin}' mknod {{root}}/mknod-probe b 4096 0 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'mknod must refuse an unencodable major with exit 1 — truncating it silently would create a node for driver 0' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'does not fit' || {{ echo \"mknod refused the major without saying so: '$e'\" >&2; exit 1; }}; \
                     [ ! -e {{root}}/mknod-probe ] || {{ echo 'a refused mknod must not have created anything' >&2; exit 1; }}; \
                     e=$('{bin}' mknod {{root}}/mknod-probe b 7 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'mknod must refuse a short operand list with exit 1' >&2; exit 1; }}; \
                     e=$('{bin}' losetup -r dev/loop0 /img 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'losetup must refuse a relative path with exit 1' >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // init's inittab validator, through the argv[0] form the kernel uses for
    // /sbin/init: a good table parses to exactly its jobs and exits 0; a table
    // with a rejected line exits 1, which is what makes an inittab typo visible
    // somewhere other than a machine that will not boot.
    //
    // The mistyped-option leg passes a VALID `--dry-run` as well, so it exits
    // either way: if the rejection ever regresses this reds on the exit status
    // instead of starting a supervision loop that hangs the check.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "d='{{root}}/argv0'; \
                     o=$(\"$d/init\" --dry-run -f '{{root}}/inittab.good') || {{ echo 'init --dry-run failed on a valid table' >&2; exit 1; }}; \
                     printf '%s\\n' \"$o\" | grep -q -x -F 'sysinit - /bin/true' || {{ echo \"init --dry-run lost the sysinit job: '$o'\" >&2; exit 1; }}; \
                     printf '%s\\n' \"$o\" | grep -q -x -F 'respawn ttyS0 /bin/sh -c exit 0' || {{ echo \"init --dry-run mis-parsed the quoted respawn job: '$o'\" >&2; exit 1; }}; \
                     printf '%s\\n' \"$o\" | grep -q -x -F 'once - /bin/echo one' || {{ echo \"init --dry-run lost the once job: '$o'\" >&2; exit 1; }}; \
                     n=$(printf '%s\\n' \"$o\" | wc -l); \
                     [ \"$n\" -eq 3 ] || {{ echo \"init --dry-run listed $n jobs, expected 3\" >&2; exit 1; }}; \
                     '{bin}' init --dry-run -f '{{root}}/inittab.bad' >/dev/null 2>&1; \
                     [ $? -eq 1 ] || {{ echo 'init --dry-run must exit 1 when a table line is rejected' >&2; exit 1; }}; \
                     e=$('{bin}' init --dryrun --dry-run -f '{{root}}/inittab.good' 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'a mistyped option must not start a supervision loop off PID 1' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'unrecognised option' || {{ echo \"init said '$e' about a mistyped option\" >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // switch_root's fail-early contract: a new root that cannot exec its init
    // must be refused BEFORE any mount moves, because a failed exec after
    // chroot(2) is an unrecoverable kernel panic rather than an error message.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "e=$('{bin}' switch_root '{{root}}/absent' /sbin/init 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'switch_root accepted a NEWROOT that does not exist' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'not a directory' || {{ echo \"switch_root said '$e' about a missing NEWROOT\" >&2; exit 1; }}; \
                     mkdir -p '{{root}}/emptyroot'; \
                     e=$('{bin}' switch_root '{{root}}/emptyroot' /sbin/init 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'switch_root accepted a new root with no init at all — it would have moved the mounts and panicked the kernel' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'does not resolve inside' || {{ echo \"switch_root said '$e' about a new root with no init\" >&2; exit 1; }}; \
                     mkdir -p '{{root}}/noexecroot/sbin'; \
                     echo 'not a program' > '{{root}}/noexecroot/sbin/init'; \
                     e=$('{bin}' switch_root '{{root}}/noexecroot' /sbin/init 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'switch_root accepted a non-executable init' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'not an executable file' || {{ echo \"switch_root said '$e' about a non-executable init\" >&2; exit 1; }}; \
                     mkdir -p '{{root}}/outside'; \
                     cp '{bin}' '{{root}}/outside/sh'; \
                     e=$('{bin}' switch_root '{{root}}/emptyroot' /../outside/sh 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'switch_root let INIT climb out of the new root' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q -F '/../outside/sh: does not resolve inside' || {{ echo \"switch_root said '$e' about an INIT escaping the new root — a REAL binary sits one '..' above the new root, so an unclamped walk reaches it. The OPERAND must be the thing refused; a message naming anything else means the walk got out\" >&2; exit 1; }}; \
                     mkdir -p '{{root}}/textroot/sbin'; \
                     echo 'not a program' > '{{root}}/textroot/sbin/init'; \
                     chmod 755 '{{root}}/textroot/sbin/init'; \
                     e=$('{bin}' switch_root '{{root}}/textroot' /sbin/init 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'switch_root accepted a chmod +x text file as INIT — exec would fail AFTER the chroot, panicking the kernel' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'neither an ELF nor' || {{ echo \"switch_root said '$e' about a non-loadable INIT\" >&2; exit 1; }}; \
                     mkdir -p '{{root}}/stubroot/sbin'; \
                     printf '\\177ELF\\002\\001\\001' > '{{root}}/stubroot/sbin/init'; \
                     chmod 755 '{{root}}/stubroot/sbin/init'; \
                     e=$('{bin}' switch_root '{{root}}/stubroot' /sbin/init 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'switch_root took four magic bytes for a program — a truncated ELF fails execve after the chroot' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'truncated' || {{ echo \"switch_root said '$e' about a truncated ELF\" >&2; exit 1; }}; \
                     mkdir -p '{{root}}/realroot/sbin'; \
                     cp '{bin}' '{{root}}/realroot/sbin/init'; \
                     e=$('{bin}' switch_root '{{root}}/realroot' /sbin/init 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'switch_root did not stop at the mount-point check' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'not a mount point' || {{ echo \"switch_root rejected a REAL static ELF as INIT: '$e' — the loadability check must accept td's own binaries\" >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // mount/umount: the rejection paths, then the read-only table listing where
    // /proc is available. NOTHING below may reach mount(2) or umount2(2) with a
    // real target — see the header. Each refusal asserts the DIAGNOSTIC as well
    // as the exit status: an unprivileged EPERM from the syscall would also exit
    // non-zero, and that would prove the applet TRIED, which is the opposite of
    // the contract these fixed argument lists depend on.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "e=$('{bin}' mount --not-an-option 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'mount must reject an unknown option with exit 1 — falling through would mount something over a directory the boot needs' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'unrecognised argument' || {{ echo \"mount rejected the option without saying so: '$e'\" >&2; exit 1; }}; \
                     e=$('{bin}' mount /mnt 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'mount accepted a lone operand — td ships no /etc/fstab to resolve one against, so it would have mounted nothing and reported success' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'fstab' || {{ echo \"mount refused a lone operand for the wrong reason: '$e'\" >&2; exit 1; }}; \
                     '{bin}' mount -t >/dev/null 2>&1; \
                     [ $? -eq 1 ] || {{ echo 'mount -t with no TYPE must exit 1, not consume the next operand' >&2; exit 1; }}; \
                     '{bin}' mount -o ro >/dev/null 2>&1; \
                     [ $? -eq 1 ] || {{ echo 'mount with options but no operands must exit 1 rather than silently printing the table' >&2; exit 1; }}; \
                     e=$('{bin}' umount 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'umount with no TARGET must exit 1' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'no TARGET' || {{ echo \"umount said '$e' about a missing TARGET\" >&2; exit 1; }}; \
                     e=$('{bin}' umount --not-an-option 2>&1); \
                     [ $? -eq 1 ] || {{ echo 'umount must reject an unknown option with exit 1 — falling through to -a would tear down the sandbox' >&2; exit 1; }}; \
                     printf '%s\\n' \"$e\" | grep -q 'unrecognised argument' || {{ echo \"umount rejected the option without saying so: '$e'\" >&2; exit 1; }}; \
                     '{bin}' umount -ax >/dev/null 2>&1; \
                     [ $? -eq 1 ] || {{ echo 'one bad letter must reject the WHOLE clustered word — applying the good ones would unmount everything on a typo' >&2; exit 1; }}; \
                     '{bin}' umount -a /proc >/dev/null 2>&1; \
                     [ $? -eq 1 ] || {{ echo 'umount -a with a TARGET is ambiguous and must be refused' >&2; exit 1; }}; \
                     if [ -r /proc/self/mounts ]; then \
                         t=$('{bin}' mount) || {{ echo 'td-init mount could not print the table' >&2; exit 1; }}; \
                         [ -n \"$t\" ] || {{ echo 'mount printed an empty table where /proc is mounted' >&2; exit 1; }}; \
                         printf '%s\\n' \"$t\" | grep -qE '^[^ ]+ on [^ ]+ type [^ ]+ [(]' || {{ echo \"mount's table is not in the 'SOURCE on TARGET type FSTYPE (OPTS)' spelling: '$t'\" >&2; exit 1; }}; \
                         n=$(printf '%s\\n' \"$t\" | grep -cE '^[^ ]+ on [^ ]+ type [^ ]+ [(]'); \
                         m=$(printf '%s\\n' \"$t\" | wc -l); \
                         [ \"$n\" -eq \"$m\" ] || {{ echo \"mount printed $m lines but only $n are mount entries\" >&2; exit 1; }}; \
                     else \
                         echo 'note: /proc not mounted in this sandbox; the mount table listing is asserted by the boot oracle'; \
                     fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // hostname: the rejection paths, then printing where /proc is mounted. The
    // SET paths are deliberately never taken — see the header.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "'{bin}' hostname -F '{{root}}/no-such-file' >/dev/null 2>&1; \
                     [ $? -eq 1 ] || {{ echo 'hostname -F must exit 1 when the file is unreadable' >&2; exit 1; }}; \
                     '{bin}' hostname one two >/dev/null 2>&1; \
                     [ $? -eq 1 ] || {{ echo 'hostname must reject two NAMEs' >&2; exit 1; }}; \
                     '{bin}' hostname -F >/dev/null 2>&1; \
                     [ $? -eq 1 ] || {{ echo 'hostname -F with no FILE must exit 1, not silently print' >&2; exit 1; }}; \
                     if [ -r /proc/sys/kernel/hostname ]; then \
                         h=$('{bin}' hostname) || {{ echo 'td-init hostname failed' >&2; exit 1; }}; \
                         [ -n \"$h\" ] || {{ echo 'hostname printed nothing' >&2; exit 1; }}; \
                         s=$('{bin}' hostname -s) || {{ echo 'td-init hostname -s failed' >&2; exit 1; }}; \
                         case \"$h\" in \"$s\"*) : ;; *) echo \"hostname -s printed '$s', which is not a prefix of '$h'\" >&2; exit 1;; esac; \
                     else \
                         echo 'note: /proc not mounted in this sandbox; hostname printing asserted by the boot oracle'; \
                     fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: td-init is a statically-linked ELF64 x86-64 executable (ET_EXEC) with no PT_INTERP and no dynamic NEEDED entry; it serves exactly the twelve applets cttyhack/halt/hostname/init/losetup/mknod/mount/poweroff/reboot/switch_root/sync/umount, dispatches through both the argv[0] and `td-init <applet>` forms, rejects an unknown reboot option before reaching reboot(2), validates an inittab through `init --dry-run` (exit 1 on a rejected line), refuses a switch_root into a new root with no executable init, refuses a non-block or unencodable mknod before reaching mknod(2), refuses an unknown mount/umount argument before reaching mount(2)/umount2(2) and a lone mount operand td has no fstab to resolve, and prints the mount table and the hostname where /proc is mounted\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("td-init-test", "1.0")
        .native_inputs(&["td-init", "binutils-x86-64-self", "busybox-x86-64"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-init-test: build-plan --auto builds td-init (td's static boot-glue multicall: init/reboot/poweroff/halt/switch_root/mount/umount/cttyhack/hostname/losetup/mknod/sync, statically linked by the /td/store target Rust + native GCC/binutils/glibc toolchain), asserts a self-contained static ELF64 x86-64 executable (ET_EXEC, no PT_INTERP, no dynamic NEEDED), and exercises the applet roster, both dispatch forms, the inittab validator, the mount-table listing, and the fail-early/reject paths of the irreversible applets"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-init-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
