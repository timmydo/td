use crate::ladder::{post_bootstrap_path, unpack_into, POST_BOOTSTRAP_SH};
use crate::types::{Recipe, Step};

// Static libcurl for Git's HTTP and HTTPS transports. This is intentionally a
// library-only output: td does not need a second interactive downloader, and a
// narrow protocol/dependency surface makes the eventual Git closure auditable.
pub fn recipe() -> Recipe {
    let sgcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let sbin = "{in:binutils-x86-64-self}/bin";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let tls = "{in:libressl-x86-64}";
    let zlib = "{in:zlib-x86-64-self}";
    let path = format!("{{root}}/wb:{{tools}}:{sbin}:{}", post_bootstrap_path());

    let mut steps = unpack_into("curl-x86-64-source", "{src}");
    steps.push(Step::ToolFarm {
        links: [
            "awk", "basename", "cat", "chmod", "cmp", "cp", "cut", "date", "diff", "dirname",
            "echo", "env", "expr", "false", "find", "grep", "head", "install", "ln", "ls", "mkdir",
            "mktemp", "mv", "printf", "pwd", "rm", "rmdir", "sed", "sort", "tail", "tee", "test",
            "touch", "tr", "true", "uname", "uniq", "wc", "which", "xargs",
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
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{POST_BOOTSTRAP_SH}\n\
             exec \"{sgcc}\" -static -isystem \"{xglibc}/include\" \
             -B\"{sbin}/\" -B\"{xglibc}/lib\" \
             -L\"{xglibc}/lib\" -static-libgcc \"$@\" \
             -fno-omit-frame-pointer -g1 \
             -ffile-prefix-map={{root}}=/td-build-root \
             -ffile-prefix-map={{src}}=/td-build \
             -ffile-prefix-map={tls}=/td-build/input/libressl \
             -ffile-prefix-map={zlib}=/td-build/input/zlib \
             -Wl,--build-id=sha1\n"
        ),
        exec: true,
    });

    steps.push(
        Step::run(
            "{src}",
            &[
                POST_BOOTSTRAP_SH,
                "./configure",
                "--build=x86_64-pc-linux-gnu",
                "--host=x86_64-pc-linux-gnu",
                "--prefix={out}",
                "--disable-shared",
                "--enable-static",
                "--enable-symbol-hiding",
                "--disable-dependency-tracking",
                "--enable-http",
                "--disable-ftp",
                "--disable-file",
                "--disable-ipfs",
                "--disable-ldap",
                "--disable-ldaps",
                "--disable-rtsp",
                "--disable-dict",
                "--disable-telnet",
                "--disable-tftp",
                "--disable-pop3",
                "--disable-imap",
                "--disable-smb",
                "--disable-smtp",
                "--disable-gopher",
                "--disable-mqtt",
                // Without libpsl, cookies can accept public-suffix supercookies.
                // Git's first transport closure keeps header authentication instead.
                "--disable-cookies",
                "--disable-manual",
                "--disable-docs",
                "--disable-libcurl-option",
                "--disable-rt",
                "--enable-threaded-resolver",
                "--disable-openssl-auto-load-config",
                "--disable-kerberos-auth",
                "--disable-negotiate-auth",
                "--disable-aws",
                "--disable-ntlm",
                "--disable-tls-srp",
                "--disable-doh",
                "--disable-alt-svc",
                "--disable-hsts",
                "--disable-websockets",
                &format!("--with-openssl={tls}"),
                &format!("--with-zlib={zlib}"),
                "--with-ca-bundle=/etc/ssl/certs/ca-certificates.crt",
                "--without-ca-path",
                "--without-ca-fallback",
                "--without-ca-embed",
                "--without-libpsl",
                "--without-libgsasl",
                "--without-libssh2",
                "--without-libssh",
                "--without-libidn2",
                "--without-nghttp2",
                "--without-ngtcp2",
                "--without-nghttp3",
                "--without-quiche",
                "--without-libuv",
                "--without-brotli",
                "--without-zstd",
                "--without-zsh-functions-dir",
                "--without-fish-functions-dir",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", POST_BOOTSTRAP_SH)
        .env("SHELL", POST_BOOTSTRAP_SH)
        .env("CC", "{root}/wb/cc")
        .env("AR", "{in:binutils-x86-64-self}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-self}/bin/ranlib")
        .env("NM", "{in:binutils-x86-64-self}/bin/nm")
        .env("CPPFLAGS", &format!("-I{tls}/include -I{zlib}/include"))
        .env("LDFLAGS", &format!("-L{tls}/lib -L{zlib}/lib"))
        .env("CFLAGS", "-O2")
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-x86-64-self}/bin/make",
                "-j{jobs}",
                "-C",
                "lib",
                "libcurl.la",
                &format!("SHELL={POST_BOOTSTRAP_SH}"),
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", POST_BOOTSTRAP_SH)
        .env("SHELL", POST_BOOTSTRAP_SH)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::MkDir {
        path: "{out}/lib".into(),
    });
    steps.push(Step::MkDir {
        path: "{out}/include/curl".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/lib/.libs/libcurl.a".into()],
        dest: "{out}/lib".into(),
    });
    steps.push(Step::CopyFiles {
        files: [
            "curl.h",
            "curlver.h",
            "easy.h",
            "header.h",
            "mprintf.h",
            "multi.h",
            "options.h",
            "stdcheaders.h",
            "system.h",
            "typecheck-gcc.h",
            "urlapi.h",
            "websockets.h",
        ]
        .iter()
        .map(|name| format!("{{src}}/include/curl/{name}"))
        .collect(),
        dest: "{out}/include/curl".into(),
    });
    // Consumers use explicit archive/header paths. Do not ship curl-config or
    // pkg-config metadata: neither interpreter is part of the target interface,
    // and the reviewed static closure is declared directly by each recipe.
    steps.push(Step::Require {
        paths: vec![
            "{out}/lib/libcurl.a".into(),
            "{out}/include/curl/curl.h".into(),
            "{out}/include/curl/curlver.h".into(),
            "{out}/include/curl/easy.h".into(),
        ],
        exec: false,
    });

    Recipe::mesboot("curl-x86-64", "8.21.0")
        .source_input("curl-x86-64-source")
        .native_inputs(&[
            "libressl-x86-64",
            "zlib-x86-64-self",
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
    use super::recipe;
    use crate::types::Step;

    #[test]
    fn static_https_build_has_the_exact_reviewed_closure() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs.as_deref(),
            Some(
                [
                    "libressl-x86-64",
                    "zlib-x86-64-self",
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
        assert!(recipe.inputs.is_none());
        let steps = recipe.steps.expect("curl steps");
        let configure = steps.iter().find_map(|step| match step {
            Step::Run { argv, .. } if argv.iter().any(|arg| arg == "./configure") => Some(argv),
            _ => None,
        });
        let configure = configure.expect("configure step");
        for required in [
            "--prefix={out}",
            "--disable-shared",
            "--enable-static",
            "--enable-http",
            "--enable-threaded-resolver",
            "--disable-cookies",
            "--with-ca-bundle=/etc/ssl/certs/ca-certificates.crt",
            "--without-ca-path",
            "--without-ca-fallback",
            "--without-ca-embed",
            "--without-libpsl",
            "--without-nghttp2",
            "--without-libssh2",
        ] {
            assert!(
                configure.iter().any(|arg| arg == required),
                "missing configure policy {required}"
            );
        }
        assert!(configure
            .iter()
            .any(|arg| arg == "--with-openssl={in:libressl-x86-64}"));
        assert!(configure
            .iter()
            .any(|arg| arg == "--with-zlib={in:zlib-x86-64-self}"));
    }

    #[test]
    fn compiler_wrapper_keeps_the_target_profile_policy_last() {
        let steps = recipe().steps.expect("curl steps");
        let wrapper = steps
            .iter()
            .find_map(|step| match step {
                Step::WriteFile { path, content, .. } if path == "{root}/wb/cc" => Some(content),
                _ => None,
            })
            .expect("compiler wrapper");
        let package_args = wrapper.find("\"$@\"").expect("package arguments");
        let profile_policy = wrapper
            .rfind("-fno-omit-frame-pointer")
            .expect("frame-pointer policy");
        assert!(package_args < profile_policy);
        for required in [
            "-g1",
            "-ffile-prefix-map={root}=/td-build-root",
            "-ffile-prefix-map={src}=/td-build",
            "-ffile-prefix-map={in:libressl-x86-64}=/td-build/input/libressl",
            "-ffile-prefix-map={in:zlib-x86-64-self}=/td-build/input/zlib",
            "-Wl,--build-id=sha1",
        ] {
            assert!(
                wrapper.contains(required),
                "missing target policy {required}"
            );
        }
    }
}
