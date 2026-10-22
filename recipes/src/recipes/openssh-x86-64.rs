use crate::shared::{
    build_root_dynamic_cc, build_root_link_map_check, unpack_into, LinkedLibc, BUILD_ROOT_PATH,
    BUILD_ROOT_SH as SH,
};
use crate::types::{Recipe, Step};

// OpenSSH Portable supplies td's SSH client and daemon from one pinned source.
// This is deliberately the no-libcrypto profile: the negotiated surface is
// restricted by the image configuration to the internal post-quantum hybrid
// KEXs, Ed25519 keys, and ChaCha20-Poly1305. zlib, PAM, Kerberos, PKCS#11,
// FIDO, libedit, login accounting, and the unshipped client/server tools never
// enter the closure.
//
// It compiles in the Debian build root against td glibc under the shipped
// target profile (`build_root_dynamic_cc`; the seccomp sandbox's linux/
// headers are td glibc's staged copy). Each program links td glibc's libc.so.6
// with its loader and DT_RUNPATH, and gcc-runtime's libgcc; its link map
// proves no input came from the root, and the wrapper's guard puts the start
// files in gcc-runtime and the sysroot. configure's resolved library lines
// are pinned to td glibc's libraries. Every program is split into a runtime
// and a debug companion (td-profiler/DESIGN.md §2).
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

/// The shipped programs, each built by its own make.
const PROGRAMS: &[&str] = &["ssh", "sshd", "sshd-session", "sshd-auth", "ssh-keygen"];

pub fn recipe() -> Recipe {
    let path = format!("{{root}}/wb:{BUILD_ROOT_PATH}");

    let mut steps = unpack_into("openssh-x86-64-source", "{src}");
    steps.push(Step::WriteFile {
        path: "{src}/auth-passwd.c".into(),
        content: AUTH_PASSWORD_DISABLED_C.into(),
        exec: false,
    });
    steps.push(Step::MkDir {
        path: "{root}/build".into(),
    });
    steps.extend(build_root_dynamic_cc());
    steps.push(
        Step::run(
            "{root}/build",
            &[
                SH,
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
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("AR", "/usr/bin/ar")
        .env("RANLIB", "/usr/bin/ranlib")
        .env("NM", "/usr/bin/nm")
        .env("OBJCOPY", "/usr/bin/objcopy")
        .env("CFLAGS", "-O2")
        .env("LDFLAGS", "-Wl,--build-id=sha1")
        // td glibc has no crypt(3); the probe would find the root's libcrypt.
        .env("ac_cv_lib_crypt_crypt", "no")
        .env("ac_cv_func_crypt", "no")
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    // A probe that finds a library outside td glibc changes the build even
    // where no shipped link records it; only td glibc's may be resolved.
    steps.push(
        Step::run(
            "{root}/build",
            &[
                SH,
                "-c",
                "set -e; for v in LIBS CHANNELLIBS K5LIBS GSSLIBS SSHDLIBS LIBEDIT LIBFIDO2 LIBWTMPDB; do \
                 line=$(grep \"^$v=\" Makefile) || { echo \"Makefile has no $v\" >&2; exit 1; }; \
                 for t in ${line#*=}; do case $t in \
                 -lresolv|-lutil|-ldl|-lrt|-lpthread|-lm) ;; \
                 *) echo \"configure resolved $v: $t\" >&2; exit 1;; esac; done; done",
            ],
        )
        .env("PATH", &path),
    );
    // One make per program: each link writes, and is checked against, its own
    // map.
    for program in PROGRAMS {
        let map = format!("{{root}}/{program}.map");
        steps.push(
            Step::run(
                "{root}/build",
                &["/usr/bin/make", "-j{jobs}", &format!("SHELL={SH}"), program],
            )
            .env("PATH", &path)
            .env("SHELL", SH)
            .env("TD_LINK_MAP", &map)
            .env("SOURCE_DATE_EPOCH", "1"),
        );
        steps.push(build_root_link_map_check(
            "{root}/build",
            &map,
            program,
            &["gcc-runtime"],
            LinkedLibc::Shared,
        ));
    }
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
    steps.push(Step::split_debug_tree("{out}", "/usr/bin/objcopy"));

    Recipe::mesboot("openssh-x86-64", "10.5p1")
        .source_input("openssh-x86-64-source")
        .build_root()
        .native_inputs(&["glibc-x86-64", "gcc-runtime"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::{recipe, AUTH_PASSWORD_DISABLED_C, PROGRAMS};
    use crate::types::Step;

    #[test]
    fn build_is_the_bounded_no_libcrypto_client_server_profile() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs.as_deref(),
            Some(
                ["glibc-x86-64", "gcc-runtime"]
                    .map(str::to_string)
                    .as_slice()
            )
        );
        assert!(recipe.build_root);
        let steps = recipe.steps.unwrap_or_default();
        let configure = steps.iter().find_map(|step| match step {
            Step::Run { argv, .. } if argv.iter().any(|arg| arg.ends_with("/configure")) => {
                Some(argv)
            }
            _ => None,
        });
        let configure = configure.expect("OpenSSH configure invocation");
        let crypt_off = steps.iter().any(|step| {
            matches!(step, Step::Run { argv, env, .. }
            if argv.iter().any(|arg| arg.ends_with("/configure"))
                && ["ac_cv_lib_crypt_crypt", "ac_cv_func_crypt"].iter().all(|var| {
                    env.iter().any(|(key, value)| key == var && value == "no")
                }))
        });
        assert!(
            crypt_off,
            "configure must not find the build root's libcrypt"
        );
        let build_id = steps.iter().any(|step| {
            matches!(step, Step::Run { argv, env, .. }
                if argv.iter().any(|arg| arg.ends_with("/configure"))
                    && env.iter().any(|(key, value)| key == "LDFLAGS"
                        && value.contains("-Wl,--build-id=sha1")))
        });
        assert!(
            build_id,
            "configure's LDFLAGS must carry the SHA-1 build id"
        );
        let libraries_pinned = steps.iter().any(|step| {
            matches!(step, Step::Run { argv, .. }
                if argv.iter().any(|arg| arg.contains("for v in LIBS CHANNELLIBS")
                    && arg.contains("SSHDLIBS")))
        });
        assert!(libraries_pinned, "configure's library lines must be pinned");
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
            assert!(
                configure.iter().any(|arg| arg == required),
                "missing {required}"
            );
        }
        for forbidden in ["libressl-x86-64", "zlib-x86-64", "rust-toolchain"] {
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

        let builds: Vec<_> = steps
            .iter()
            .filter_map(|step| match step {
                Step::Run { argv, env, .. }
                    if argv.first().is_some_and(|arg| arg.ends_with("/bin/make")) =>
                {
                    Some((argv, env))
                }
                _ => None,
            })
            .collect();
        assert_eq!(builds.len(), PROGRAMS.len(), "one make per program");
        for target in ["ssh", "sshd", "sshd-session", "sshd-auth", "ssh-keygen"] {
            let map = format!("{{root}}/{target}.map");
            let (link, _) = steps
                .iter()
                .enumerate()
                .find(|(_, step)| {
                    matches!(step, Step::Run { argv, env, .. }
                        if argv.last().is_some_and(|arg| arg == target)
                            && env.iter().any(|(key, value)| key == "TD_LINK_MAP" && *value == map))
                })
                .expect("the program's own make writes its map");
            let checked = steps
                .iter()
                .position(|step| {
                    matches!(step, Step::Run { argv, .. }
                        if argv.iter().any(|arg| arg.contains(&format!("m='{map}'"))
                            && arg.contains(&format!("= '{target}'"))
                            && arg.contains("$sys/libc.so.6")))
                })
                .expect("link-map check");
            assert!(link < checked, "{target}'s map is checked after its link");
        }
        for omitted in ["scp", "sftp", "sftp-server", "ssh-agent", "ssh-add"] {
            assert!(
                !builds
                    .iter()
                    .any(|(argv, _)| argv.iter().any(|arg| arg == omitted)),
                "built {omitted}"
            );
        }

        assert!(AUTH_PASSWORD_DISABLED_C.contains("return 0;"));
        assert!(!AUTH_PASSWORD_DISABLED_C.contains("crypt("));
    }

    #[test]
    fn compiler_policy_follows_package_flags() {
        crate::shared::assert_build_root_dynamic_c(&recipe());
    }
}
