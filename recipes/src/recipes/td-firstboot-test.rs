use crate::ladder::{post_bootstrap_path, POST_BOOTSTRAP_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// td-firstboot-test: build-shape AND behavioural validation of the per-machine
// identity provisioner.
//
// Per repo policy that recipes test their output, this asserts the shipped
// td-firstboot binary is the self-contained STATIC ELF its slot requires,
// re-proving with an independent readelf walk what the producer's `assert_static`
// fail-closes on:
//   1. ELF64 x86-64 *executable* (readelf: class ELF64, machine x86-64, type
//      EXEC) — EXEC (not DYN) is the non-PIE static shape,
//   2. NO PT_INTERP program header,
//   3. NO dynamic NEEDED entry — an EMPTY runtime closure.
//
// It then EXERCISES provisioning, which is the part worth testing: the program's
// whole contract is that it mints an identity ONCE and never silently replaces it.
// So the behavioural legs run it TWICE over the same state directory and assert the
// first run reports NEW while the second reports STABLE with the identical host-key
// fingerprint — the same before/after shape the qemu oracle checks across a reboot,
// but decided here in seconds and without a VM.
//
// `--keygen` is pointed at a STUB rather than the real sshd: sshd is a heavy
// `--auto` rust recipe, and what needs proving here is td-firstboot's end of the
// contract (the argv it passes, the reply it parses, the files it then insists on).
// The real pairing of the two programs is exercised by `qemu-boot-system`, which
// boots the image where /bin/sshd is the actual daemon. The stub is deliberately
// also driven into its failure modes — a keygen that exits non-zero, and one that
// claims success without writing a key — because those are the paths where a
// machine ends up believing it has an identity it does not have.
//
// The persistent-storage refusal (a state dir on tmpfs) is NOT tested here: it is a
// pure decision over /proc/mounts text, unit-tested in td-firstboot's own
// `mounts` module, and the build sandbox cannot present an arbitrary mount table.
pub fn recipe() -> Recipe {
    let post_bootstrap_shebang = format!("#!{POST_BOOTSTRAP_SH}");
    let bin = "{in:td-firstboot}/bin/td-firstboot";
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let mut steps = Vec::new();

    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "h=$('{readelf}' -h '{bin}' 2>/dev/null) || {{ echo 'readelf -h failed on td-firstboot' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || {{ echo 'td-firstboot is not ELF64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || {{ echo 'td-firstboot is not x86-64' >&2; exit 1; }}; \
                     printf '%s\\n' \"$h\" | grep -qE 'Type:[[:space:]]+EXEC([[:space:]]|$)' || {{ echo 'td-firstboot is not a static ET_EXEC - a DYN/PIE would need runtime relocation, and this runs at sysinit before the machine has an identity to report a failure with' >&2; exit 1; }}"
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
                    "lout=$('{readelf}' -l '{bin}' 2>/dev/null) || {{ echo 'readelf -l failed on td-firstboot (cannot verify absence of PT_INTERP)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$lout\" | grep -qi 'INTERP'; then echo 'td-firstboot carries a PT_INTERP program header - it is not static' >&2; exit 1; fi; \
                     dout=$('{readelf}' -d '{bin}' 2>/dev/null) || {{ echo 'readelf -d failed on td-firstboot (cannot verify absence of dynamic NEEDED)' >&2; exit 1; }}; \
                     if printf '%s\\n' \"$dout\" | grep -qi 'NEEDED'; then echo 'td-firstboot has a dynamic NEEDED entry - its runtime closure is not empty' >&2; exit 1; fi"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // The keygen stub: sshd's `keygen` contract, in the smallest form that can hold
    // it. Writes a fixed key pair and reports `created`; on a second call it finds
    // the private key already there and reports `existing` with the same
    // fingerprint, exactly as the real daemon does.
    steps.push(Step::WriteFile {
        path: "{root}/keygen-stub".into(),
        content: format!(
            "{post_bootstrap_shebang}\n{}",
            "# usage: <self> keygen --host-key PATH --public-key PATH\n\
             priv=; pub=\n\
             while [ $# -gt 0 ]; do\n\
               case $1 in\n\
                 --host-key) priv=$2; shift 2 ;;\n\
                 --public-key) pub=$2; shift 2 ;;\n\
                 *) shift ;;\n\
               esac\n\
             done\n\
             [ -n \"$priv\" ] && [ -n \"$pub\" ] || { echo 'stub: missing paths' >&2; exit 1; }\n\
             if [ -f \"$priv\" ]; then echo 'existing SHA256:stubfingerprint'; exit 0; fi\n\
             printf 'STUB PRIVATE KEY\\n' > \"$priv\"\n\
             printf 'ssh-ed25519 STUB stub\\n' > \"$pub\"\n\
             echo 'created SHA256:stubfingerprint'\n"
        ),
        exec: true,
    });
    // A keygen that fails, and one that lies about having written a key. Both must
    // make td-firstboot refuse rather than report an identity the machine lacks.
    steps.push(Step::WriteFile {
        path: "{root}/keygen-fails".into(),
        content: format!("{post_bootstrap_shebang}\necho 'stub: no entropy' >&2\nexit 3\n"),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/keygen-lies".into(),
        content: format!("{post_bootstrap_shebang}\necho 'created SHA256:stubfingerprint'\n"),
        exec: true,
    });

    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "state='{{root}}/state'; stub='{{root}}/keygen-stub'; \
                     err=\"{{root}}/first.err\"; \
                     first=$('{bin}' provision --state-dir \"$state\" --keygen \"$stub\" 2>\"$err\") || \
                       {{ echo 'td-firstboot provision failed on a fresh state dir; its own diagnostic:' >&2; cat \"$err\" >&2; exit 1; }}; \
                     printf '%s\\n' \"$first\" | grep -q -x -F TD-FIRSTBOOT-NEW-OK || \
                       {{ echo \"first provision did not report a first boot: $first\" >&2; exit 1; }}; \
                     for f in machine-id ssh/ssh_host_ed25519_key ssh/ssh_host_ed25519_key.pub ssh/authorized_keys; do \
                       [ -f \"$state/$f\" ] || {{ echo \"td-firstboot did not create $f\" >&2; exit 1; }}; \
                     done; \
                     grep -qE '^[0-9a-f]{{32}}$' \"$state/machine-id\" || \
                       {{ echo 'the machine-id is not 32 lowercase hex digits, which is the one shape every reader expects' >&2; exit 1; }}; \
                     id=$(cat \"$state/machine-id\"); \
                     while read -r line; do \
                       case \"$line\" in ''|'#'*) : ;; *) echo 'a fresh authorized_keys authorizes somebody - it must deny all' >&2; exit 1;; esac; \
                     done < \"$state/ssh/authorized_keys\"; \
                     second=$('{bin}' provision --state-dir \"$state\" --keygen \"$stub\" 2>\"$err\") || \
                       {{ echo 'the second provision failed - it must be idempotent; its own diagnostic:' >&2; cat \"$err\" >&2; exit 1; }}; \
                     printf '%s\\n' \"$second\" | grep -q -x -F TD-FIRSTBOOT-STABLE-OK || \
                       {{ echo \"the second provision did not report a stable identity: $second\" >&2; exit 1; }}; \
                     printf '%s\\n' \"$second\" | grep -q -x -F TD-FIRSTBOOT-NEW-OK && \
                       {{ echo 'the second provision reported a FIRST boot - the identity was re-minted, which is the failure this program exists to prevent' >&2; exit 1; }}; \
                     [ \"$(cat \"$state/machine-id\")\" = \"$id\" ] || \
                       {{ echo 'the machine-id changed between boots' >&2; exit 1; }}; \
                     for pair in \"machine-id -r--r--r--\" \"ssh/ssh_host_ed25519_key -rw-------\" \
                                 \"ssh/ssh_host_ed25519_key.pub -rw-r--r--\" \"ssh/authorized_keys -rw-------\"; do \
                       set -- $pair; \
                       got=$(ls -l \"$state/$1\" | cut -c1-10); \
                       [ \"$got\" = \"$2\" ] || {{ echo \"$1 has mode $got, expected $2 - td-firstboot must repair what keygen wrote (the private key must not be group/world readable, and the .pub must be)\" >&2; exit 1; }}; \
                     done; \
                     dirmode=$(ls -ld \"$state/ssh\" | cut -c1-10); \
                     [ \"$dirmode\" = drwx--x--x ] || {{ echo \"the key directory has mode $dirmode, expected drwx--x--x - it must be TRAVERSABLE (an unprivileged read of the .pub is asserted at boot) but not listable\" >&2; exit 1; }}; \
                     f1=$(printf '%s\\n' \"$first\"  | grep TD-FIRSTBOOT-HOSTKEY); \
                     f2=$(printf '%s\\n' \"$second\" | grep TD-FIRSTBOOT-HOSTKEY); \
                     [ -n \"$f1\" ] && [ \"$f1\" = \"$f2\" ] || \
                       {{ echo \"the host-key fingerprint line changed between boots: '$f1' then '$f2'\" >&2; exit 1; }}"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );

    // The refusals. A machine that believes it has an identity it does not have is
    // worse than one that fails to provision, so each of these must exit non-zero
    // and emit NO marker.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "for case in fails lies; do \
                       out=$('{bin}' provision --state-dir \"{{root}}/state-$case\" --keygen \"{{root}}/keygen-$case\" 2>/dev/null); \
                       st=$?; \
                       [ \"$st\" -ne 0 ] || {{ echo \"td-firstboot accepted a keygen that $case\" >&2; exit 1; }}; \
                       printf '%s\\n' \"$out\" | grep -q TD-FIRSTBOOT && \
                         {{ echo \"td-firstboot emitted a marker after a keygen that $case: $out\" >&2; exit 1; }}; \
                     done; \
                     '{bin}' --nonesuch >/dev/null 2>&1; \
                     [ $? -eq 2 ] || {{ echo 'td-firstboot must exit 2 on an unknown argument (usage), not provision something unasked' >&2; exit 1; }}; \
                     '{bin}' --help >/dev/null || {{ echo 'td-firstboot --help failed' >&2; exit 1; }}; \
                     bad='{{root}}/state-corrupt'; mkdir -p \"$bad\"; \
                     printf 'not-a-machine-id\\n' > \"$bad/machine-id\"; \
                     '{bin}' provision --state-dir \"$bad\" --keygen '{{root}}/keygen-stub' >/dev/null 2>&1; \
                     [ $? -ne 0 ] || {{ echo 'td-firstboot replaced a malformed machine-id instead of refusing - a corrupt read must never become a new machine identity' >&2; exit 1; }}; \
                     [ \"$(cat \"$bad/machine-id\")\" = not-a-machine-id ] || \
                       {{ echo 'td-firstboot overwrote the malformed machine-id it refused' >&2; exit 1; }}"
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
        content: "PASS: td-firstboot is a statically-linked ELF64 x86-64 executable (ET_EXEC) with no PT_INTERP and no dynamic NEEDED entry; a first provision mints a 32-hex machine-id, an SSH host key pair, and a deny-all authorized_keys, reporting TD-FIRSTBOOT-NEW-OK; a second provision over the same state reports TD-FIRSTBOOT-STABLE-OK with an unchanged machine-id and host-key fingerprint; and it refuses (non-zero, no marker) a keygen that fails, a keygen that claims success without writing a key, an unknown argument, and a malformed machine-id it will not replace\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("td-firstboot-test", "1.0")
        .native_inputs(&[
            "td-firstboot",
            "binutils-x86-64-self",
            "busybox-x86-64",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check td-firstboot-test: build-plan --auto builds td-firstboot (td's static per-machine identity provisioner: machine-id, SSH host key, deny-all authorized_keys under /var/lib/td, which the image's /etc reaches through reviewed per-file symlinks), asserts a self-contained static ELF64 x86-64 executable (ET_EXEC, no PT_INTERP, no dynamic NEEDED), and exercises provisioning twice to prove it mints an identity once and never re-mints it, plus its refusals"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-firstboot-test daily 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
