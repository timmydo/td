use crate::ladder::{post_bootstrap_path, split_target_debug, unpack_into, POST_BOOTSTRAP_SH};
use crate::types::{Recipe, Step};

// OpenSSH Portable supplies td's SSH client and daemon from one pinned source.
// This is deliberately the no-libcrypto profile: the negotiated surface is
// restricted by the image configuration to the internal post-quantum hybrid
// KEXs, Ed25519 keys, and ChaCha20-Poly1305. zlib, PAM, Kerberos, PKCS#11,
// FIDO, libedit, login accounting, and the unshipped client/server tools never
// enter the closure.
//
// Portable OpenSSH has no build switch that removes password verification, and
// its fallback verifier requires crypt(3) even when PasswordAuthentication is
// disabled at runtime. Replace that one translation unit with a fail-closed
// implementation. This both keeps libcrypt out and makes the image's
// public-key-only authentication policy true in the linked server, not just in
// sshd_config.
const AUTH_PASSWORD_DISABLED_C: &str = r#"#include "includes.h"
#include <sys/types.h>
#include "hostfile.h"
#include "auth.h"

int
auth_password(struct ssh *ssh, const char *password)
{
	(void)ssh;
	(void)password;
	return 0;
}
"#;

pub fn recipe() -> Recipe {
    let sgcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let sbin = "{in:binutils-x86-64-self}/bin";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let path = format!("{{root}}/wb:{{tools}}:{sbin}:{}", post_bootstrap_path());

    let mut steps = unpack_into("openssh-x86-64-source", "{src}");
    steps.push(Step::ToolFarm {
        links: [
            "awk", "basename", "cat", "chmod", "cmp", "cp", "cut", "date", "diff",
            "dirname", "echo", "egrep", "env", "expr", "false", "find", "grep", "head",
            "install", "ln", "ls", "mkdir", "mktemp", "mv", "printf", "pwd", "rm",
            "rmdir", "sed", "sort", "tail", "tee", "test", "touch", "tr", "true",
            "uname", "wc", "which", "xargs",
        ]
        .iter()
        .map(|name| ((*name).into(), "{in:busybox-x86-64}/bin/busybox".into()))
        .collect(),
    });
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: POST_BOOTSTRAP_SH.into(),
    });
    steps.push(Step::WriteFile {
        path: "{src}/auth-passwd.c".into(),
        content: AUTH_PASSWORD_DISABLED_C.into(),
        exec: false,
    });
    steps.push(Step::MkDir {
        path: "{root}/build".into(),
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{POST_BOOTSTRAP_SH}\n\
             exec \"{sgcc}\" -isystem \"{xglibc}/include\" \
             -B\"{sbin}/\" -B\"{xglibc}/lib\" \
             -L\"{xglibc}/lib\" \"$@\" \
             -static-libgcc -fno-omit-frame-pointer -g1 \
             -ffile-prefix-map={{root}}=/td-build-root \
             -ffile-prefix-map={{src}}=/td-build \
             -Wl,--dynamic-linker -Wl,{xglibc}/lib/ld-linux-x86-64.so.2 \
             -Wl,--enable-new-dtags -Wl,-rpath -Wl,{xglibc}/lib \
             -Wl,--build-id=sha1\n"
        ),
        exec: true,
    });
    steps.push(
        Step::run(
            "{root}/build",
            &[
                POST_BOOTSTRAP_SH,
                "{src}/configure",
                "--build=x86_64-pc-linux-gnu",
                "--host=x86_64-pc-linux-gnu",
                "--prefix={out}",
                "--bindir={out}/bin",
                "--sbindir={out}/bin",
                "--libexecdir={out}/libexec",
                "--sysconfdir=/etc/ssh",
                "--with-default-path=/bin",
                "--with-pid-dir=/run",
                "--with-privsep-user=sshd",
                "--with-privsep-path=/run/sshd-empty",
                "--with-sandbox=seccomp_filter",
                "--without-openssl",
                "--without-zlib",
                "--without-pam",
                "--without-kerberos5",
                "--without-selinux",
                "--without-ldns",
                "--without-libedit",
                "--without-xauth",
                "--disable-pkcs11",
                "--disable-security-key",
                "--disable-lastlog",
                "--disable-utmp",
                "--disable-utmpx",
                "--disable-wtmp",
                "--disable-wtmpx",
                "--disable-libutil",
                "--disable-etc-default-login",
                "--disable-strip",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", POST_BOOTSTRAP_SH)
        .env("SHELL", POST_BOOTSTRAP_SH)
        .env("CC", "{root}/wb/cc")
        .env("AR", "{in:binutils-x86-64-self}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-self}/bin/ranlib")
        .env("NM", "{in:binutils-x86-64-self}/bin/nm")
        .env("OBJCOPY", "{in:binutils-x86-64-self}/bin/objcopy")
        .env("CFLAGS", "-O2")
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{root}/build",
            &[
                "{in:make-x86-64-self}/bin/make",
                "-j{jobs}",
                &format!("SHELL={POST_BOOTSTRAP_SH}"),
                "ssh",
                "sshd",
                "sshd-session",
                "sshd-auth",
                "ssh-keygen",
            ],
        )
        .env("PATH", &path)
        .env("SHELL", POST_BOOTSTRAP_SH)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    for dir in ["{out}/bin", "{out}/libexec"] {
        steps.push(Step::MkDir { path: dir.into() });
    }
    steps.push(Step::CopyFiles {
        files: ["ssh", "sshd", "ssh-keygen"]
            .iter()
            .map(|name| format!("{{root}}/build/{name}"))
            .collect(),
        dest: "{out}/bin".into(),
    });
    steps.push(Step::CopyFiles {
        files: ["sshd-session", "sshd-auth"]
            .iter()
            .map(|name| format!("{{root}}/build/{name}"))
            .collect(),
        dest: "{out}/libexec".into(),
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/bin/ssh".into(),
            "{out}/bin/sshd".into(),
            "{out}/bin/ssh-keygen".into(),
            "{out}/libexec/sshd-session".into(),
            "{out}/libexec/sshd-auth".into(),
        ],
        exec: true,
    });
    steps.push(split_target_debug("{out}"));

    Recipe::mesboot("openssh-x86-64", "10.5p1")
        .source_input("openssh-x86-64-source")
        .native_inputs(&[
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "make-x86-64-self",
            "busybox-x86-64",
        ])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::{recipe, AUTH_PASSWORD_DISABLED_C};
    use crate::types::Step;

    #[test]
    fn build_is_the_bounded_no_libcrypto_client_server_profile() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs.as_deref(),
            Some(
                [
                    "gcc-x86-64-self",
                    "binutils-x86-64-self",
                    "glibc-x86-64",
                    "make-x86-64-self",
                    "busybox-x86-64",
                ]
                .map(str::to_string)
                .as_slice()
            )
        );
        let steps = recipe.steps.unwrap_or_default();
        let configure = steps.iter().find_map(|step| match step {
            Step::Run { argv, .. }
                if argv.iter().any(|arg| arg.ends_with("/configure")) =>
            {
                Some(argv)
            }
            _ => None,
        });
        let configure = configure.expect("OpenSSH configure invocation");
        for required in [
            "--without-openssl",
            "--without-zlib",
            "--without-pam",
            "--disable-pkcs11",
            "--disable-security-key",
            "--with-sandbox=seccomp_filter",
            "--with-privsep-user=sshd",
            "--with-privsep-path=/run/sshd-empty",
        ] {
            assert!(configure.iter().any(|arg| arg == required), "missing {required}");
        }
        for forbidden in ["libressl-x86-64", "zlib-x86-64-self", "rust-toolchain"] {
            assert!(
                !recipe
                    .native_inputs
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .any(|input| input == forbidden),
                "minimal OpenSSH unexpectedly depends on {forbidden}"
            );
        }

        let build = steps.iter().find_map(|step| match step {
            Step::Run { argv, .. }
                if argv.first().is_some_and(|arg| arg.ends_with("/bin/make")) =>
            {
                Some(argv)
            }
            _ => None,
        });
        let build = build.expect("bounded OpenSSH make invocation");
        for target in ["ssh", "sshd", "sshd-session", "sshd-auth", "ssh-keygen"] {
            assert!(build.iter().any(|arg| arg == target), "missing {target}");
        }
        for omitted in ["scp", "sftp", "sftp-server", "ssh-agent", "ssh-add"] {
            assert!(!build.iter().any(|arg| arg == omitted), "built {omitted}");
        }

        assert!(AUTH_PASSWORD_DISABLED_C.contains("return 0;"));
        assert!(!AUTH_PASSWORD_DISABLED_C.contains("crypt("));
    }

    #[test]
    fn compiler_policy_follows_package_flags() {
        let wrapper = recipe()
            .steps
            .unwrap_or_default()
            .into_iter()
            .find_map(|step| match step {
                Step::WriteFile { path, content, .. } if path == "{root}/wb/cc" => {
                    Some(content)
                }
                _ => None,
            })
            .expect("compiler wrapper");
        let package_args = wrapper.find("\"$@\"").expect("package flags");
        let policy = wrapper
            .rfind("-fno-omit-frame-pointer")
            .expect("target profile");
        assert!(package_args < policy);
        for required in [
            "-static-libgcc",
            "-g1",
            "-ffile-prefix-map={root}=/td-build-root",
            "-ffile-prefix-map={src}=/td-build",
            "-Wl,--build-id=sha1",
        ] {
            assert!(wrapper.contains(required), "missing {required}");
        }
    }
}
