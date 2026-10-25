use crate::shared::{BUILD_ROOT_PATH, BUILD_ROOT_SH as SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

pub fn recipe() -> Recipe {
    let openssh = "{in:openssh-x86-64}";
    let readelf = "/usr/bin/readelf";
    let path = format!("{openssh}/bin:{BUILD_ROOT_PATH}");
    let binaries = [
        format!("{openssh}/bin/ssh"),
        format!("{openssh}/bin/sshd"),
        format!("{openssh}/bin/ssh-keygen"),
        format!("{openssh}/libexec/sshd-session"),
        format!("{openssh}/libexec/sshd-auth"),
    ];
    let debug = [
        format!("{openssh}/lib/debug/bin/ssh.debug"),
        format!("{openssh}/lib/debug/bin/sshd.debug"),
        format!("{openssh}/lib/debug/bin/ssh-keygen.debug"),
        format!("{openssh}/lib/debug/libexec/sshd-session.debug"),
        format!("{openssh}/lib/debug/libexec/sshd-auth.debug"),
    ];
    let policy =
        |form, provisioned| super::system_x86_64::ssh_policy::config("alice", form, provisioned);
    let desktop = super::system_x86_64::ssh_policy::Provisioned::No;
    let server = super::system_x86_64::ssh_policy::Provisioned::Server;
    let ordinary = super::system_x86_64::ssh_policy::Form::Ordinary;
    let enforced = super::system_x86_64::ssh_policy::Form::Enforced;

    let mut steps = vec![
        Step::Require {
            paths: binaries.to_vec(),
            exec: true,
        },
        Step::Require {
            paths: debug.to_vec(),
            exec: false,
        },
        Step::MkDir {
            path: "{root}/home".into(),
        },
        Step::MkDir {
            path: "{root}/keys".into(),
        },
        Step::WriteFile {
            path: "{root}/sshd_config".into(),
            content: policy(ordinary, desktop),
            exec: false,
        },
        Step::WriteFile {
            path: "{root}/sshd_config_enforced".into(),
            content: policy(enforced, desktop),
            exec: false,
        },
        Step::WriteFile {
            path: "{root}/sshd_config_server".into(),
            content: policy(ordinary, server),
            exec: false,
        },
        Step::WriteFile {
            path: "{root}/sshd_config_server_enforced".into(),
            content: policy(enforced, server),
            exec: false,
        },
    ];
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "for binary in {}; do \
                         header=$('{readelf}' -h \"$binary\") || exit 1; \
                         printf '%s\\n' \"$header\" | grep -Fq 'Class:                             ELF64' || exit 1; \
                         printf '%s\\n' \"$header\" | grep -Fq 'Machine:                           Advanced Micro Devices X86-64' || exit 1; \
                         program=$('{readelf}' -l \"$binary\") || exit 1; \
                         printf '%s\\n' \"$program\" | grep -Fq '{{in:glibc-x86-64}}/stage/td/store/glibc-2.41-x86_64/lib/ld-linux-x86-64.so.2' || \
                             {{ echo \"OpenSSH does not use the final glibc interpreter: $binary\" >&2; exit 1; }}; \
                         dynamic=$('{readelf}' -d \"$binary\") || exit 1; \
                         needed=$(printf '%s\\n' \"$dynamic\" | sed -n 's/.*Shared library: \\[\\(.*\\)\\]/\\1/p') || exit 1; \
                         test \"$needed\" = libc.so.6 || \
                             {{ echo \"OpenSSH has an unexpected dynamic dependency set: $binary: $needed\" >&2; exit 1; }}; \
                         runpath=$(printf '%s\\n' \"$dynamic\" | sed -n 's/.*Library runpath: \\[\\(.*\\)\\]/\\1/p') || exit 1; \
                         test \"$runpath\" = '{{in:glibc-x86-64}}/stage/td/store/glibc-2.41-x86_64/lib' || \
                             {{ echo \"OpenSSH has an unexpected RUNPATH: $binary: $runpath\" >&2; exit 1; }}; \
                         if printf '%s\\n' \"$dynamic\" | grep -Fq '(RPATH)'; then \
                             echo \"OpenSSH carries legacy RPATH: $binary\" >&2; exit 1; \
                         fi; \
                         for forbidden in libcrypto.so libssl.so libz.so libcrypt.so '/gnu/store' '/td-input/'; do \
                             if grep -a -Fq \"$forbidden\" \"$binary\"; then \
                                 echo \"OpenSSH retains forbidden runtime input $forbidden: $binary\" >&2; exit 1; \
                             fi; \
                         done; \
                     done",
                    binaries.join(" ")
                ),
            ],
        )
        .env("PATH", &path),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "v=$('{openssh}/bin/ssh' -V 2>&1) || exit 1; \
                     case \"$v\" in OpenSSH_10.5p1*) :;; *) echo \"unexpected ssh version: $v\" >&2; exit 1;; esac; \
                     kex=$('{openssh}/bin/ssh' -Q kex) || exit 1; \
                     for algorithm in mlkem768x25519-sha256 sntrup761x25519-sha512 curve25519-sha256; do \
                         printf '%s\\n' \"$kex\" | grep -q -x -F \"$algorithm\" || \
                             {{ echo \"OpenSSH omits KEX $algorithm\" >&2; exit 1; }}; \
                     done; \
                     ciphers=$('{openssh}/bin/ssh' -Q cipher) || exit 1; \
                     printf '%s\\n' \"$ciphers\" | grep -q -x -F chacha20-poly1305@openssh.com || exit 1; \
                     keys=$('{openssh}/bin/ssh' -Q key-plain) || exit 1; \
                     printf '%s\\n' \"$keys\" | grep -q -x -F ssh-ed25519 || exit 1; \
                     if printf '%s\\n' \"$keys\" | grep -Eq '^(ssh-rsa|ecdsa-sha2-)'; then \
                         echo 'no-libcrypto OpenSSH unexpectedly exposes RSA/ECDSA keys' >&2; exit 1; \
                     fi; \
                     '{openssh}/bin/ssh-keygen' -q -t ed25519 -N '' -C td-openssh-test \
                         -f '{{root}}/keys/id_ed25519' || exit 1; \
                     fp=$('{openssh}/bin/ssh-keygen' -l -E sha256 \
                         -f '{{root}}/keys/id_ed25519.pub') || exit 1; \
                     case \"$fp\" in '256 SHA256:'*' (ED25519)') :;; \
                         *) echo \"unexpected Ed25519 fingerprint: $fp\" >&2; exit 1;; esac"
                ),
            ],
        )
        .env("PATH", &path)
        .env("HOME", "{root}/home"),
    );
    steps.push(
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "for form in sshd_config sshd_config_enforced sshd_config_server \
                         sshd_config_server_enforced; do \
                     case \"$form\" in sshd_config|sshd_config_server) admission='PermitRootLogin no';; \
                         *) admission=$(printf '%s\\n%s' 'PermitRootLogin no' 'AllowUsers alice');; esac; \
                     for user in alice tester root; do for addr in 127.0.0.1 192.0.2.20; do \
                         policy=$('{openssh}/bin/sshd' -h '{{root}}/keys/id_ed25519' \
                             -T -f \"{{root}}/$form\" \
                             -C \"user=$user,host=localhost,addr=$addr\") || exit 1; \
                         case \"$form:$user\" in sshd_config:alice) expected='{selftest} .ssh/authorized_keys';; \
                             sshd_config_enforced:alice) expected='{selftest}';; \
                             sshd_config_server:alice) expected='{selftest} {provisioned} .ssh/authorized_keys';; \
                             sshd_config_server_enforced:alice) expected='{selftest} {provisioned}';; \
                             *) expected='{persistent}';; esac; \
                         printf '%s\\n' \"$policy\" | grep -q -x -F \
                             \"AuthorizedKeysFile $expected\" || \
                             {{ printf '%s\\n' \"unexpected authorization for $user\" \"$policy\" >&2; exit 1; }}; \
                         admitted=$(printf '%s\\n' \"$policy\" | grep -E '^(PermitRootLogin|AllowUsers|DenyUsers|AllowGroups|DenyGroups) ') || exit 1; \
                         test \"$admitted\" = \"$admission\" || \
                             {{ printf '%s\\n' \"unexpected admission in $form for $user\" \"$admitted\" >&2; exit 1; }}; \
                     done; done; \
                     done",
                    selftest = super::system_x86_64::ssh_policy::SSHD_SELFTEST_AUTHORIZED_KEYS,
                    persistent = super::system_x86_64::ssh_policy::SSHD_AUTHORIZED_KEYS,
                    provisioned = super::system_x86_64::ssh_policy::SSHD_PROVISIONED_AUTHORIZED_KEYS,
                ),
            ],
        )
        .env("PATH", &path),
    );
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: OpenSSH Portable 10.5p1 provides the bounded ssh/sshd/ssh-keygen profile configured for seccomp_filter, with Ed25519, ML-KEM/SNTRUP/Curve25519 KEX, and ChaCha20-Poly1305; the built daemon checks the shared primary-account configuration, in its ordinary and enforced forms and their server variants, with an ephemeral test host key, ordinary-only owner-managed remote keys, the server's root-owned provisioned keys in both its forms, distinct primary/persistent authorization paths for loopback and remote addresses, and each form's effective PermitRootLogin (no, in both forms) and AllowUsers lines for each user (sshd -T reports the configuration; it attempts no login); every shipped ELF uses only td glibc and has a debug companion; libcrypto, libcrypt, zlib, and agent/PKCS#11/FIDO/SCP/SFTP-server binaries are absent\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    // glibc-x86-64 is the programs' loader; the inspection tools are the
    // root's binutils, which built nothing here.
    Recipe::mesboot("openssh-x86-64-test", "1.0")
        .build_root()
        .native_inputs(&["openssh-x86-64", "glibc-x86-64"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check openssh-x86-64-test: build minimal OpenSSH and prove its exact crypto/runtime/debug surface"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run openssh-x86-64-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::recipe;

    #[test]
    fn validation_uses_only_the_built_client_server_and_the_roots_tools() {
        let recipe = recipe();
        crate::shared::assert_build_root_test(&recipe, &["openssh-x86-64", "glibc-x86-64"], &[]);
        assert_eq!(
            recipe.native_inputs.as_deref(),
            Some(
                ["openssh-x86-64", "glibc-x86-64"]
                    .map(str::to_string)
                    .as_slice()
            )
        );
        assert!(recipe.inputs.is_none());
    }
}
