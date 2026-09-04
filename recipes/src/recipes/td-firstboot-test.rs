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
// `--keygen` is pointed at a STUB rather than the real OpenSSH package: what needs
// proving here is td-firstboot's end of the standard `ssh-keygen` contract (the
// generation and fingerprint argv, and the files it then insists on). The real
// pairing is exercised by `qemu-boot-system`, which boots the image with the actual
// `/bin/ssh-keygen` and `/bin/sshd`. The stub is deliberately
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
    // The two programs the provisioned configurations are for, pinned at the
    // commits the image ships: the proof that a template parses is the
    // program's own parser, not a second reading of its format.
    let tmc = "{in:tmc}/bin/tmc";
    let tn = "{in:tn}/bin/tn";
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

    // The keygen stub: the three standard OpenSSH operations td-firstboot uses, in
    // the smallest form that can hold them. Generation writes a fixed pair,
    // derivation parses the private fixture and emits its public half, and
    // fingerprinting reports a fixed 256-bit Ed25519 SHA-256 line.
    steps.push(Step::WriteFile {
        path: "{root}/keygen-stub".into(),
        content: format!(
            "{post_bootstrap_shebang}\n{}",
            "# generation: -q -t ed25519 -N '' -C COMMENT -f PATH\n\
             # derivation: -y -P '' -f PATH\n\
             # fingerprint: -l -E sha256 -f PATH.pub\n\
             mode=generate; file=\n\
             while [ $# -gt 0 ]; do\n\
               case $1 in\n\
                 -l) mode=fingerprint; shift ;;\n\
                 -y) mode=derive; shift ;;\n\
                 -f) file=$2; shift 2 ;;\n\
                 -t|-N|-C|-E|-P) shift 2 ;;\n\
                 *) shift ;;\n\
               esac\n\
             done\n\
             [ -n \"$file\" ] || { echo 'stub: missing -f path' >&2; exit 1; }\n\
             if [ \"$mode\" = derive ]; then\n\
               key=$(cat \"$file\") || exit 1\n\
               case \"$key\" in\n\
                 'STUB PRIVATE KEY') echo 'ssh-ed25519 STUB td-openssh-host-key' ;;\n\
                 'STUB RSA PRIVATE KEY') echo 'ssh-rsa RSASTUB wrong-type' ;;\n\
                 *) echo 'stub: malformed private key' >&2; exit 1 ;;\n\
               esac\n\
               exit 0\n\
             fi\n\
             if [ \"$mode\" = fingerprint ]; then\n\
               [ -f \"$file\" ] || exit 1\n\
               echo '256 SHA256:stubfingerprint td-openssh-host-key (ED25519)'\n\
               exit 0\n\
             fi\n\
             printf 'STUB PRIVATE KEY\\n' > \"$file\"\n\
             printf 'ssh-ed25519 STUB td-openssh-host-key\\n' > \"$file.pub\"\n"
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
        content: format!("{post_bootstrap_shebang}\nexit 0\n"),
        exec: true,
    });
    steps.push(Step::WriteFile {
        path: "{root}/keygen-bad-fingerprint".into(),
        content: format!(
            "{post_bootstrap_shebang}\n{}",
            "mode=generate; file=\n\
             while [ $# -gt 0 ]; do\n\
               case $1 in\n\
                 -l) mode=fingerprint; shift ;;\n\
                 -y) mode=derive; shift ;;\n\
                 -f) file=$2; shift 2 ;;\n\
                 -t|-N|-C|-E|-P) shift 2 ;;\n\
                 *) shift ;;\n\
               esac\n\
             done\n\
             case \"$mode\" in\n\
               derive) echo 'ssh-ed25519 STUB td-openssh-host-key' ;;\n\
               fingerprint) echo 'not a SHA-256 Ed25519 fingerprint' ;;\n\
               generate) printf 'STUB PRIVATE KEY\\n' > \"$file\"; \
                 printf 'ssh-ed25519 STUB td-openssh-host-key\\n' > \"$file.pub\" ;;\n\
             esac\n"
        ),
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

    // The terminal applications' first configuration. The flag pair adds the
    // mail and news configurations under the login user's td-jail state: once,
    // owned by that user, in the modes the jail insists on, and without turning
    // a stable identity into a first boot. The sandbox user's own uid is the
    // owner td-firstboot is given, so the chown root performs at boot runs here
    // as the permitted no-op rather than being skipped; the hand-over to a
    // different identity is what the boot oracle's application markers prove.
    // Then the pinned programs read what was provisioned: tmc starts offline
    // in its CLI mode and exits cleanly at end of input, and tn loads the feed
    // list and reaches for the feeds, which either answer or fail to, both of
    // which mean the configuration parsed (a parse failure is its own
    // message). Each finds its file as it does inside the jail: tmc through
    // the configuration home, tn through an explicit path.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "state='{{root}}/state'; stub='{{root}}/keygen-stub'; err=\"{{root}}/apps.err\"; \
                     home='{{root}}/home'; mkdir -p \"$home\" && chmod 700 \"$home\" || exit 1; \
                     set -- $(ls -lnd \"$home\"); owner=\"$3:$4\"; \
                     out=$('{bin}' provision --state-dir \"$state\" --keygen \"$stub\" --application-home \"$home\" --application-owner \"$owner\" 2>\"$err\") || \
                       {{ echo 'provision with the application pair failed; its own diagnostic:' >&2; cat \"$err\" >&2; exit 1; }}; \
                     printf '%s\\n' \"$out\" | grep -q -x -F TD-FIRSTBOOT-STABLE-OK || \
                       {{ echo \"provisioning the applications turned a stable identity into something else: $out\" >&2; exit 1; }}; \
                     for f in mail/config/tmc/config.toml mail/config/tmc/password news/config/tn/config.toml; do \
                       [ -f \"$home/.td/app/$f\" ] || {{ echo \"td-firstboot did not provision $f\" >&2; exit 1; }}; \
                       got=$(ls -l \"$home/.td/app/$f\" | cut -c1-10); \
                       [ \"$got\" = -rw------- ] || {{ echo \"$f has mode $got, expected -rw------- - the jail binds only private state\" >&2; exit 1; }}; \
                       set -- $(ls -ln \"$home/.td/app/$f\"); [ \"$3:$4\" = \"$owner\" ] || {{ echo \"$f is owned by $3:$4, not $owner\" >&2; exit 1; }}; \
                     done; \
                     for d in .td .td/app .td/app/mail .td/app/mail/config .td/app/mail/config/tmc .td/app/news .td/app/news/config .td/app/news/config/tn; do \
                       got=$(ls -ld \"$home/$d\" | cut -c1-10); \
                       [ \"$got\" = drwx------ ] || {{ echo \"$d has mode $got, expected drwx------\" >&2; exit 1; }}; \
                       set -- $(ls -lnd \"$home/$d\"); [ \"$3:$4\" = \"$owner\" ] || {{ echo \"$d is owned by $3:$4, not $owner\" >&2; exit 1; }}; \
                     done; \
                     grep -q -F 'password_file = \"/home/td/.config/tmc/password\"' \"$home/.td/app/mail/config/tmc/config.toml\" || \
                       {{ echo 'the mail configuration does not name the password file at its jail-side path' >&2; exit 1; }}; \
                     grep -q -x -F '[[feed]]' \"$home/.td/app/news/config/tn/config.toml\" || \
                       {{ echo 'the news configuration has no feed, and tn refuses to start without one' >&2; exit 1; }}; \
                     HOME=\"$home\" XDG_CONFIG_HOME=\"$home/.td/app/mail/config\" '{tmc}' --offline --cli < /dev/null > '{{root}}/tmc.out' 2> '{{root}}/tmc.err' || \
                       {{ echo 'the provisioned mail configuration does not start tmc offline; its own diagnostic:' >&2; cat '{{root}}/tmc.err' >&2; exit 1; }}; \
                     HOME=\"$home\" '{tn}' --config \"$home/.td/app/news/config/tn/config.toml\" --cache '{{root}}/tn-cache' --fetch-and-quit > '{{root}}/tn.out' 2> '{{root}}/tn.err'; \
                     grep -q 'Error loading config' '{{root}}/tn.err' && \
                       {{ echo 'the provisioned news configuration does not load in tn; its own diagnostic:' >&2; cat '{{root}}/tn.err' >&2; exit 1; }}; \
                     grep -Eq 'Fetch error|^Fetched ' '{{root}}/tn.err' || \
                       {{ echo 'tn did not reach for its feeds from the provisioned configuration; its own diagnostic:' >&2; cat '{{root}}/tn.err' >&2; exit 1; }}; \
                     printf 'edited\\n' > \"$home/.td/app/mail/config/tmc/config.toml\"; \
                     rm \"$home/.td/app/mail/config/tmc/password\"; \
                     out=$('{bin}' provision --state-dir \"$state\" --keygen \"$stub\" --application-home \"$home\" --application-owner \"$owner\" 2>\"$err\") || \
                       {{ echo 'a later provision over edited application state failed; its own diagnostic:' >&2; cat \"$err\" >&2; exit 1; }}; \
                     [ \"$(cat \"$home/.td/app/mail/config/tmc/config.toml\")\" = edited ] || \
                       {{ echo 'a later provision rewrote an operator-edited application configuration' >&2; exit 1; }}; \
                     [ -f \"$home/.td/app/mail/config/tmc/password\" ] || \
                       {{ echo 'a later provision did not restore a missing sibling file' >&2; exit 1; }}; \
                     '{bin}' provision --state-dir \"$state\" --keygen \"$stub\" --application-home \"$home\" >/dev/null 2>&1; \
                     [ $? -eq 2 ] || {{ echo 'td-firstboot must exit 2 (usage) when --application-home comes without --application-owner' >&2; exit 1; }}; \
                     '{bin}' provision --state-dir \"$state\" --keygen \"$stub\" --application-home \"$home\" --application-owner 0:0 >/dev/null 2>&1; \
                     [ $? -eq 2 ] || {{ echo 'td-firstboot must exit 2 (usage) for a root application owner' >&2; exit 1; }}; \
                     out=$('{bin}' provision --state-dir \"$state\" --keygen \"$stub\" --application-home \"{{root}}/nobody\" --application-owner \"$owner\" 2>\"$err\") || \
                       {{ echo 'an absent application home must be skipped with a diagnostic, not failed: the identity does not depend on a mail client' >&2; exit 1; }}; \
                     grep -q 'is absent' \"$err\" || {{ echo 'the skipped application home was not reported' >&2; exit 1; }}; \
                     [ ! -e '{{root}}/nobody' ] || {{ echo 'td-firstboot created an application home it was not given' >&2; exit 1; }}"
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
                    "for case in fails lies bad-fingerprint; do \
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
                       {{ echo 'td-firstboot overwrote the malformed machine-id it refused' >&2; exit 1; }}; \
                     for case in malformed wrong-type mismatch; do \
                       state=\"{{root}}/state-$case-pair\"; mkdir -p \"$state/ssh\" || exit 1; \
                       private=\"$state/ssh/ssh_host_ed25519_key\"; public=\"$private.pub\"; \
                       case \"$case\" in \
                         malformed) printf 'NOT A PRIVATE KEY\\n' > \"$private\"; \
                           printf 'ssh-ed25519 STUB recorded\\n' > \"$public\" ;; \
                         wrong-type) printf 'STUB RSA PRIVATE KEY\\n' > \"$private\"; \
                           printf 'ssh-rsa RSASTUB recorded\\n' > \"$public\" ;; \
                         mismatch) printf 'STUB PRIVATE KEY\\n' > \"$private\"; \
                           printf 'ssh-ed25519 OTHER recorded\\n' > \"$public\" ;; \
                       esac; \
                       chmod 0600 \"$private\"; chmod 0644 \"$public\"; \
                       out=$('{bin}' provision --state-dir \"$state\" \
                         --keygen '{{root}}/keygen-stub' 2>/dev/null); st=$?; \
                       [ \"$st\" -ne 0 ] || \
                         {{ echo \"td-firstboot accepted a $case complete host-key pair\" >&2; exit 1; }}; \
                       printf '%s\\n' \"$out\" | grep -q TD-FIRSTBOOT && \
                         {{ echo \"td-firstboot emitted a marker for a $case host-key pair: $out\" >&2; exit 1; }}; \
                     done; \
                     for missing in private public; do \
                       state=\"{{root}}/state-missing-$missing\"; mkdir -p \"$state/ssh\" || exit 1; \
                       private=\"$state/ssh/ssh_host_ed25519_key\"; public=\"$private.pub\"; \
                       printf 'STUB PRIVATE KEY\\n' > \"$private\"; \
                       printf 'ssh-ed25519 STUB recorded\\n' > \"$public\"; \
                       chmod 0600 \"$private\"; chmod 0644 \"$public\"; \
                       case \"$missing\" in private) rm \"$private\" ;; public) rm \"$public\" ;; esac; \
                       out=$('{bin}' provision --state-dir \"$state\" \
                         --keygen '{{root}}/keygen-stub' 2>/dev/null); st=$?; \
                       [ \"$st\" -ne 0 ] || \
                         {{ echo \"td-firstboot accepted a host-key pair missing its $missing half\" >&2; exit 1; }}; \
                       printf '%s\\n' \"$out\" | grep -q TD-FIRSTBOOT && \
                         {{ echo \"td-firstboot emitted a marker for a pair missing $missing: $out\" >&2; exit 1; }}; \
                     done; \
                     :"
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
        content: "PASS: td-firstboot is a statically-linked ELF64 x86-64 executable (ET_EXEC) with no PT_INTERP and no dynamic NEEDED entry; a first provision mints a 32-hex machine-id, an SSH host key pair, and a deny-all authorized_keys, reporting TD-FIRSTBOOT-NEW-OK; a second provision over the same state reports TD-FIRSTBOOT-STABLE-OK with an unchanged machine-id and host-key fingerprint; and it refuses (non-zero, no marker) a keygen that fails, writes nothing, or reports a malformed fingerprint, an unknown argument, a malformed machine-id, incomplete keypairs, and malformed, wrong-type, or mismatched complete host-key pairs; and with --application-home/--application-owner it provisions the mail (tmc) and news (tn) configurations once under HOME/.td/app as 0700 directories and 0600 files owned by that user, reports the identity STABLE rather than NEW, never rewrites an edited file and restores a missing one, refuses half the pair or a root owner (exit 2), skips an absent home with a diagnostic, and starts the pinned tmc (offline, CLI mode) and tn (feed load) from the provisioned files\n".into(),
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
            "tmc",
            "tn",
        ])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check td-firstboot-test: build-plan --auto builds td-firstboot (td's static per-machine identity provisioner: machine-id, SSH host key, deny-all authorized_keys under /var/lib/td, which the image's /etc reaches through reviewed per-file symlinks), asserts a self-contained static ELF64 x86-64 executable (ET_EXEC, no PT_INTERP, no dynamic NEEDED), and exercises provisioning twice to prove it mints an identity once and never re-mints it, plus its refusals; then proves the application pair provisions the mail and news configurations once, privately, under the login user's jail state, and that the pinned tmc and tn start from them"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run td-firstboot-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
