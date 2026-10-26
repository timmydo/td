use crate::shared::{build_root_static_cc, unpack_into, BUILD_ROOT_PATH, BUILD_ROOT_SH as SH};
use crate::types::{Recipe, Step};

// Static libcurl for Git's HTTP and HTTPS transports. This is intentionally a
// library-only output: td does not need a second interactive downloader, and a
// narrow protocol/dependency surface makes the eventual Git closure auditable.
// It compiles in the Debian build root against td glibc
// (`build_root_static_cc`); Git's link consumes the archive.
pub fn recipe() -> Recipe {
    let tls = "{in:libressl-x86-64}";
    let zlib = "{in:zlib-x86-64}";
    let path = format!("{{root}}/wb:{BUILD_ROOT_PATH}");
    let cflags = format!(
        "-O2 -ffile-prefix-map={tls}=/td-build/input/libressl \
         -ffile-prefix-map={zlib}=/td-build/input/zlib"
    );

    let mut steps = unpack_into("curl-x86-64-source", "{src}");
    steps.extend(build_root_static_cc());

    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
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
                "--disable-doh",
                "--disable-alt-svc",
                "--disable-hsts",
                "--disable-websockets",
                "--disable-httpsig",
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
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("AR", "/usr/bin/ar")
        .env("RANLIB", "/usr/bin/ranlib")
        .env("NM", "/usr/bin/nm")
        .env("CPPFLAGS", &format!("-I{tls}/include -I{zlib}/include"))
        .env("LDFLAGS", &format!("-L{tls}/lib -L{zlib}/lib"))
        .env("CFLAGS", &cflags)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "/usr/bin/make",
                "-j{jobs}",
                "-C",
                "lib",
                "libcurl.la",
                &format!("SHELL={SH}"),
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
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

    Recipe::mesboot("curl-x86-64", "8.23.0")
        .source_input("curl-x86-64-source")
        .build_root()
        .native_inputs(&[
            "libressl-x86-64",
            "zlib-x86-64",
            "glibc-x86-64",
            "gcc-runtime",
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
                    "zlib-x86-64",
                    "glibc-x86-64",
                    "gcc-runtime",
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
            .any(|arg| arg == "--with-zlib={in:zlib-x86-64}"));
    }

    #[test]
    fn build_root_wrapper_and_cflags_keep_the_target_profile() {
        let recipe = recipe();
        crate::shared::assert_build_root_static_c(&recipe);
        let cflags = recipe
            .steps
            .unwrap_or_default()
            .into_iter()
            .find_map(|step| match step {
                Step::Run { argv, env, .. } if argv.iter().any(|arg| arg == "./configure") => env
                    .into_iter()
                    .find_map(|(key, value)| (key == "CFLAGS").then_some(value)),
                _ => None,
            })
            .expect("configure CFLAGS");
        for required in [
            "-ffile-prefix-map={in:libressl-x86-64}=/td-build/input/libressl",
            "-ffile-prefix-map={in:zlib-x86-64}=/td-build/input/zlib",
        ] {
            assert!(cflags.contains(required), "missing {required}");
        }
    }
}
