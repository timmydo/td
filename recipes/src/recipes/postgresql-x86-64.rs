use crate::shared::{
    build_root_dynamic_cc, build_root_link_map_cc, build_root_link_maps_check, unpack_into,
    LinkedLibc, BUILD_ROOT_PATH, BUILD_ROOT_SH as SH,
};
use crate::types::{Recipe, Step};

// PostgreSQL 17, the database td's mail server stores everything in, with
// the client programs its operators run (psql, pg_dump, pg_restore,
// pg_basebackup, pg_receivewal, pg_verifybackup) and contrib's pg_trgm,
// which the mail schema creates. postgresql-x86-64-test runs it; no image
// holds it yet.
//
// It compiles in the Debian build root against td glibc under the shipped
// target profile (`build_root_dynamic_cc`). The release tarball carries no
// generated sources, so the root's bison, flex and perl make the parser,
// scanner and catalog files from it. Each link writes its own map
// (`build_root_link_map_cc`), and every map must show td glibc,
// gcc-runtime's libgcc and the build's own objects alone. zlib is td's
// static libz.a, copied under the build directory so that no output names
// zlib's store path. Every ELF is split into a runtime and a debug companion
// (TRUST.md, "Target user-mode profiling contract").
//
// Without OpenSSL, which td does not build, the server has no TLS: it serves
// its Unix socket, where the mail server authenticates with SCRAM, and no
// network client the mail server would accept; it refuses `ssl = on`.
//
// The server's pg_config view reports configure's arguments and the
// compiler as upstream compiles them in, so it names the fixed scratch
// paths of this build (`{root}/wb/map-cc`, `{root}/zlib`): text, no store
// reference. Time zones are PostgreSQL's own bundled database, compiled by
// its zic into share/timezone, so they move with this pin rather than with
// td's tzdata recipe, which would otherwise be a runtime input.

/// configure's switches: every optional library off but zlib.
const CONFIGURE: &[&str] = &[
    "--build=x86_64-pc-linux-gnu",
    "--host=x86_64-pc-linux-gnu",
    "--prefix={out}",
    "--with-zlib",
    "--without-openssl",
    "--without-icu",
    "--without-readline",
    "--without-llvm",
    "--without-lz4",
    "--without-zstd",
    "--without-libxml",
    "--without-libxslt",
    "--without-gssapi",
    "--without-ldap",
    "--without-pam",
    "--without-bsd-auth",
    "--without-bonjour",
    "--without-selinux",
    "--without-systemd",
    "--without-perl",
    "--without-python",
    "--without-tcl",
    "--disable-nls",
];

/// The links 17.12 makes, each through `map-cc`: the server, its loadable
/// modules, libpq, the client programs, pg_trgm and the build-only programs.
const LINKS_AT_LEAST: usize = 77;

/// The ELF files that ship once the development tree is gone.
const SHIPPED_AT_LEAST: usize = 63;

/// What `make install` writes that serves only compiling against
/// PostgreSQL, which no td recipe does: headers, static archives,
/// pkg-config files, pg_config, PGXS (whose makefiles name this build's
/// scratch compiler), and ecpg, whose programs need those headers.
const DEVELOPMENT: &[&str] = &[
    "{out}/include",
    "{out}/lib/pgxs",
    "{out}/lib/pkgconfig",
    "{out}/bin/pg_config",
    "{out}/bin/ecpg",
    "glob:{out}/lib/*.a",
    "glob:{out}/lib/libecpg*",
    "glob:{out}/lib/libpgtypes*",
];

/// The programs the mail server and its operators run.
pub const PROGRAMS: &[&str] = &[
    "postgres",
    "initdb",
    "pg_ctl",
    "psql",
    "pg_dump",
    "pg_dumpall",
    "pg_restore",
    "pg_basebackup",
    "pg_receivewal",
    "pg_verifybackup",
    "pg_waldump",
    "pg_controldata",
    "pg_isready",
];

pub fn recipe() -> Recipe {
    let zlib = "{in:zlib-x86-64}";
    let path = format!("{{root}}/wb:{BUILD_ROOT_PATH}");
    let maps = "{root}/maps";

    let mut steps = unpack_into("postgresql-x86-64-source", "{src}");
    steps.extend(build_root_dynamic_cc());
    steps.push(build_root_link_map_cc());
    for dir in ["{root}/zlib/lib", "{root}/zlib/include", maps] {
        steps.push(Step::MkDir { path: dir.into() });
    }
    steps.push(Step::CopyFiles {
        files: vec![format!("{zlib}/lib/libz.a")],
        dest: "{root}/zlib/lib".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec![
            format!("{zlib}/include/zlib.h"),
            format!("{zlib}/include/zconf.h"),
        ],
        dest: "{root}/zlib/include".into(),
    });
    let mut configure = vec![SH, "{src}/configure"];
    configure.extend_from_slice(CONFIGURE);
    steps.push(
        Step::run("{src}", &configure)
            .env("PATH", &path)
            .env("CONFIG_SHELL", SH)
            .env("SHELL", SH)
            .env("CC", "{root}/wb/map-cc")
            .env("CFLAGS", "-O2")
            .env("CPPFLAGS", "-I{root}/zlib/include")
            .env("LDFLAGS", "-L{root}/zlib/lib -Wl,--build-id=sha1")
            .env("SOURCE_DATE_EPOCH", "1"),
    );
    // A probe that finds a library outside td glibc and the copied zlib
    // changes the build even where no link records it, and a feature the
    // switches leave out must stay out.
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
                "-c",
                "set -e; line=$(grep '^LIBS = ' src/Makefile.global) || { echo 'Makefile.global has no LIBS' >&2; exit 1; }; \
                 z=0; for t in ${line#LIBS = }; do case $t in -lz) z=1;; -lm) ;; \
                 *) echo \"configure resolved LIBS: $t\" >&2; exit 1;; esac; done; \
                 [ $z = 1 ] || { echo 'configure did not take zlib' >&2; exit 1; }; \
                 h=src/include/pg_config.h; grep -qx '#define HAVE_LIBZ 1' $h || { echo 'pg_config.h lacks zlib' >&2; exit 1; }; \
                 for d in USE_OPENSSL USE_ICU HAVE_LIBREADLINE HAVE_LIBEDIT USE_LZ4 USE_ZSTD USE_LLVM ENABLE_NLS \
                 USE_LIBXML USE_LIBXSLT USE_LDAP USE_PAM ENABLE_GSS USE_SYSTEMD HAVE_LIBSELINUX USE_BONJOUR USE_BSD_AUTH; do \
                 if grep -q \"^#define $d \" $h; then echo \"configure enabled $d\" >&2; exit 1; fi; done",
            ],
        )
        .env("PATH", &path),
    );
    // Every link, install's included, writes its map.
    let make = |dir: &str, target: Option<&str>| {
        let mut argv = vec!["/usr/bin/make", "-j{jobs}"];
        if dir != "." {
            argv.extend_from_slice(&["-C", dir]);
        }
        argv.extend(target);
        Step::run("{src}", &argv)
            .env("PATH", &path)
            .env("TD_LINK_MAPS", maps)
            .env("SOURCE_DATE_EPOCH", "1")
    };
    steps.push(make(".", None));
    steps.push(make("contrib/pg_trgm", None));
    steps.push(make(".", Some("install")));
    steps.push(make("contrib/pg_trgm", Some("install")));
    steps.push(build_root_link_maps_check(
        "{src}",
        maps,
        LINKS_AT_LEAST,
        &["gcc-runtime"],
        LinkedLibc::Shared,
    ));
    steps.push(Step::Require {
        paths: [
            "{out}/include/libpq-fe.h",
            "{out}/lib/pgxs/src/Makefile.global",
            "{out}/lib/libpq.a",
            "{out}/lib/libecpg.so",
        ]
        .map(String::from)
        .to_vec(),
        exec: false,
    });
    let mut prune = vec!["/usr/bin/rm", "-r", "-f"];
    prune.extend_from_slice(DEVELOPMENT);
    steps.push(Step::run("{out}", &prune).env("PATH", BUILD_ROOT_PATH));
    // Every ELF that ships is, byte for byte, the output of a link whose map
    // was checked above (install copies without stripping).
    steps.push(
        Step::run(
            "{out}",
            &[
                SH,
                "-c",
                &format!(
                    "set -e; shopt -s globstar dotglob nullglob; elves=0; src=$(realpath -m '{{src}}'); \
                     for f in '{{out}}'/**; do [ -f \"$f\" ] && [ ! -L \"$f\" ] || continue; \
                     case $(od -An -N4 -tx1 \"$f\") in ' 7f 45 4c 46') ;; *) continue;; esac; \
                     elves=$((elves+1)); b=${{f##*/}}; found=0; \
                     for m in '{maps}'/*.map; do n=${{m##*/}}; n=${{n%.map}}; \
                     [ \"${{n##*+}}\" = \"$b\" ] || continue; \
                     if cmp -s \"$f\" \"$src/${{n//+/\\/}}\"; then found=1; fi; done; \
                     [ $found = 1 ] || {{ echo \"$f ships without a checked link map\" >&2; exit 1; }}; \
                     done; \
                     [ $elves -ge {SHIPPED_AT_LEAST} ] || {{ echo \"only $elves ELF files ship\" >&2; exit 1; }}; \
                     if grep -r -a -l -F '{{in:zlib-x86-64}}' '{{out}}'; then \
                     echo 'the output names zlib-x86-64' >&2; exit 1; fi; \
                     echo \"$elves shipped ELF files have checked link maps\""
                ),
            ],
        )
        .env("PATH", BUILD_ROOT_PATH),
    );
    steps.push(Step::Require {
        paths: PROGRAMS
            .iter()
            .map(|program| format!("{{out}}/bin/{program}"))
            .collect(),
        exec: true,
    });
    steps.push(Step::Require {
        paths: [
            "{out}/lib/libpq.so.5",
            "{out}/lib/plpgsql.so",
            "{out}/lib/pg_trgm.so",
            "{out}/share/postgres.bki",
            "{out}/share/extension/pg_trgm.control",
            "{out}/share/extension/pg_trgm--1.5--1.6.sql",
            "{out}/share/timezone/UTC",
        ]
        .map(String::from)
        .to_vec(),
        exec: false,
    });
    steps.push(Step::split_debug_tree("{out}", "/usr/bin/objcopy"));

    Recipe::mesboot("postgresql-x86-64", "17.12")
        .source_input("postgresql-x86-64-source")
        .build_root()
        .native_inputs(&["zlib-x86-64", "glibc-x86-64", "gcc-runtime"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::{recipe, CONFIGURE, DEVELOPMENT};
    use crate::types::Step;

    #[test]
    fn build_takes_zlib_and_leaves_every_other_library_out() {
        let recipe = recipe();
        assert_eq!(
            recipe.native_inputs.as_deref(),
            Some(
                ["zlib-x86-64", "glibc-x86-64", "gcc-runtime"]
                    .map(str::to_string)
                    .as_slice()
            )
        );
        assert!(recipe.build_root);
        assert!(recipe.inputs.is_none());
        let steps = recipe.steps.unwrap_or_default();
        let (configure, env) = steps
            .iter()
            .find_map(|step| match step {
                Step::Run { argv, env, .. } if argv.iter().any(|a| a.ends_with("/configure")) => {
                    Some((argv, env))
                }
                _ => None,
            })
            .expect("configure");
        for flag in CONFIGURE {
            assert!(configure.iter().any(|arg| arg == flag), "missing {flag}");
        }
        // `--with-ssl` takes a library and has no `--without` form; the
        // obsolete boolean spelling says no, and the configure pin refuses
        // USE_OPENSSL.
        assert!(!configure.iter().any(|arg| arg.starts_with("--with-ssl")));
        for library in [
            "openssl", "icu", "readline", "llvm", "lz4", "zstd", "libxml", "gssapi", "ldap", "pam",
            "systemd", "perl", "python", "tcl",
        ] {
            let without = format!("--without-{library}");
            assert!(configure.contains(&without), "{without}");
            assert!(
                !configure
                    .iter()
                    .any(|arg| arg.starts_with(&format!("--with-{library}"))),
                "{library}"
            );
        }
        let value = |key: &str| {
            env.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
                .unwrap_or_default()
        };
        assert_eq!(value("CC"), "{root}/wb/map-cc");
        assert!(value("LDFLAGS").contains("-Wl,--build-id=sha1"));
        // The copy, never zlib's store path, so no output can name it.
        assert_eq!(value("CPPFLAGS"), "-I{root}/zlib/include");
        assert!(!value("LDFLAGS").contains("{in:zlib-x86-64}"));
    }

    /// The maps are checked after the last link, install's included; the
    /// development tree goes before the coverage check, which precedes the
    /// split, so every shipped ELF has a checked map and a companion.
    #[test]
    fn links_are_checked_before_the_development_tree_goes_and_the_split() {
        let steps = recipe().steps.unwrap_or_default();
        let position = |what: &str, found: &dyn Fn(&Step) -> bool| {
            steps
                .iter()
                .position(|step| found(step))
                .unwrap_or_else(|| panic!("{what}"))
        };
        let last_make = steps
            .iter()
            .rposition(|step| {
                matches!(step, Step::Run { argv, env, .. }
                    if argv.first().is_some_and(|a| a == "/usr/bin/make")
                        && env.iter().any(|(k, v)| k == "TD_LINK_MAPS" && v == "{root}/maps"))
            })
            .expect("make");
        let maps = position("maps check", &|step| {
            matches!(step, Step::Run { argv, .. }
                if argv.iter().any(|a| a.contains("check() { want=$2;")))
        });
        let prune = position("prune", &|step| {
            matches!(step, Step::Run { argv, .. }
                if argv.first().is_some_and(|a| a == "/usr/bin/rm")
                    && DEVELOPMENT.iter().all(|path| argv.iter().any(|a| a == path)))
        });
        let coverage = position("coverage", &|step| {
            matches!(step, Step::Run { argv, .. }
                if argv.iter().any(|a| a.contains("ships without a checked link map")))
        });
        let split = position("split", &|step| matches!(step, Step::SplitDebugTree { .. }));
        assert!(last_make < maps && maps < prune && prune < coverage && coverage < split);
    }

    #[test]
    fn compiler_policy_follows_package_flags() {
        crate::shared::assert_build_root_dynamic_c(&recipe());
    }
}
