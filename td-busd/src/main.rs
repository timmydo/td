#![forbid(unsafe_code)]

//! td-busd — td's session D-Bus broker.
//!
//! This increment is the wire format alone: the type grammar, the marshaller,
//! the demarshaller, and the bounds each refusal is measured against. Nothing
//! here opens a socket, so the crate is pure safe `std` and carries no entry in
//! `UNSAFE.md`. Serving connections needs `recvmsg`/`sendmsg`/`close`/
//! `getsockopt` — surface #10 in `APPLICATIONS.md` §D — and that syscall layer
//! is a reviewed amendment of its own rather than something this landing
//! smuggles in. `run` and `probe` are the names `APPLICATIONS.md` §A's
//! supervision units already spell; both refuse until it lands.

mod auth;
mod authscript;
mod corpus;
mod message;
mod name;
mod recorded;
mod wire;

use std::env;
use std::process;

fn usage() -> String {
    "usage: td-busd selftest".into()
}

fn dispatch(args: &[String]) -> Result<String, String> {
    match args.first().map(String::as_str) {
        Some("selftest") if args.len() == 1 => corpus::selftest(),
        Some("selftest") => Err(format!("selftest takes no arguments\n{}", usage())),
        Some(subcommand @ ("run" | "probe")) => Err(format!(
            "{subcommand} does not serve connections yet: the transport, its \
             SO_PEERCRED identity and its descriptor passing land with UNSAFE.md \
             surface #10"
        )),
        Some(other) => Err(format!("unrecognised subcommand '{other}'\n{}", usage())),
        None => Err(usage()),
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(summary) => println!("{summary}"),
        Err(failure) => {
            eprintln!("td-busd: {failure}");
            process::exit(2);
        }
    }
}

#[cfg(test)]
const SOURCES: &[(&str, &str)] = &[
    ("main", include_str!("main.rs")),
    ("auth", include_str!("auth.rs")),
    ("authscript", include_str!("authscript.rs")),
    ("corpus", include_str!("corpus.rs")),
    ("message", include_str!("message.rs")),
    ("name", include_str!("name.rs")),
    ("recorded", include_str!("recorded.rs")),
    ("wire", include_str!("wire.rs")),
];

#[cfg(test)]
fn source(module: &str) -> &'static str {
    SOURCES
        .iter()
        .find(|(name, _)| *name == module)
        .map(|(_, text)| *text)
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[test]
    fn selftest_is_the_only_subcommand_that_does_anything() {
        assert!(dispatch(&argv(&["selftest"])).is_ok());
        assert!(dispatch(&argv(&["selftest", "extra"])).is_err());
        assert!(dispatch(&argv(&[])).is_err());
        assert!(dispatch(&argv(&["monitor"])).is_err());
    }

    /// The supervision units in APPLICATIONS.md §A spell both of these. They
    /// must fail loudly rather than exit zero having served nothing: a broker
    /// that started and answered no one would satisfy `ready=` and leave every
    /// portal call hanging.
    #[test]
    fn the_supervised_subcommands_refuse_rather_than_pretend() {
        for subcommand in ["run", "probe"] {
            let refusal = match dispatch(&argv(&[subcommand])) {
                Ok(output) => panic!("{subcommand} claimed success: {output}"),
                Err(refusal) => refusal,
            };
            assert!(
                refusal.contains("surface #10"),
                "{subcommand}'s refusal does not say what is missing: {refusal}"
            );
        }
    }

    /// The crate is pure safe `std`, and this is what keeps it that way: a
    /// raw-syscall layer added here would red this test, which is the tripwire
    /// that forces the `UNSAFE.md` amendment surface #10 needs rather than
    /// letting one arrive in a diff.
    #[test]
    fn no_module_names_the_keyword_the_lint_forbids() {
        // Built at runtime so this test's own text is not what it finds.
        let keyword = format!("un{}", "safe");
        let lint = format!("{keyword}_code");
        assert_eq!(
            source("main")
                .matches(&format!("#![forbid({lint})]"))
                .count(),
            1,
            "main.rs must forbid the lint exactly once"
        );
        for (module, text) in SOURCES {
            let bare = text
                .matches(&keyword)
                .count()
                .saturating_sub(text.matches(&lint).count());
            assert_eq!(bare, 0, "{module} names the {keyword} keyword");
        }
    }

    /// A module missing from `SOURCES` is one the scan above cannot see.
    #[test]
    fn the_scan_covers_every_module_the_crate_declares() {
        let declared: Vec<&str> = source("main")
            .lines()
            .filter_map(|line| {
                line.trim()
                    .strip_prefix("mod ")
                    .and_then(|rest| rest.strip_suffix(';'))
            })
            .collect();
        assert!(!declared.is_empty(), "no module declarations were found");
        for module in &declared {
            assert!(
                SOURCES.iter().any(|(name, _)| name == module),
                "{module} is declared but not scanned"
            );
        }
        assert_eq!(
            declared.len() + 1,
            SOURCES.len(),
            "SOURCES lists something the crate does not declare"
        );
    }
}
