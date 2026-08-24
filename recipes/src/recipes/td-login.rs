use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

// td-login — target-built static multicall for td's credential switch.
//
// This recipe compiles the td-login CRATE's binary source (`src/main.rs` plus its
// sibling modules) into a statically-linked target ELF. The sources are embedded
// via `include_str!` so the lintable/testable crate and the shipped binary are
// ONE source of truth and cannot drift; the path escapes the
// `recipes/src/recipes/*.rs` catalog glob, so it is not itself a recipe module.
//
// SCOPE: the busybox applets that CHANGE WHO A PROCESS IS — `login` (what getty
// execs, through /etc/autologin) and `su` (what every unprivileged health leg on
// the image goes through). They are one binary because they are one operation
// with two front ends: resolve an account, decide whether a session may start,
// then switch credentials once, in one place, in one order.
//
// td-login/THREAT-MODEL.md is the specification, and the reason this is not
// simply "another applet moved off busybox": a credential-ordering bug here is
// privilege escalation. The crate confines its `unsafe` to one `syscall2` body
// under a scoped `#[allow]` beneath a crate-level `deny`, carrying exactly three
// syscalls — setgroups(116), setgid(106), setuid(105) — issued once each, in
// that order, from one function, with the result read back out of
// /proc/self/status before anything execs. That is the FOURTH target-side unsafe
// exception recorded in UNSAFE.md, after td-kexec, td-netd and td-init, and the
// crate's `mod confinement` tests pin every part of it the compiler cannot.
//
// Why not `CommandExt::uid()/gid()/groups()`, which would need no unsafe at all:
// `groups` is unstable on the pinned stable rustc (feature(setgroups)), so the
// only reachable std behaviour DROPS every supplementary group — a user in
// `wheel` would silently lose it — and std applies credentials in a forked child
// where nothing can read back what actually took. The readback is the whole
// defence; see THREAT-MODEL.md section 2.
//
// system-x86-64 SHIPS this as the /bin/{login,su} farm, off busybox. Unlike the
// td-util cutover the success paths need no synthetic probe: `login -f` is how
// the image reaches its greeter, and `su` is how /etc/rootcheck and
// /etc/bootsuccess run every unprivileged leg, so a regression in either fails
// the boot outright. What the boot could NOT see is a switch that "worked" while
// leaving a residual credential behind — every marker still prints — so the
// health target additionally runs `td-login verify-credentials` THROUGH `su` and
// gates TD-LOGIN-RUN-OK on the kernel's own readback.
//
// Why mesboot-style (rustc invoked directly) rather than `Recipe::rust`, and why
// static: identical to td-sh/td-util/td-init/td-kexec. `login` is the program
// that stands between a console and a session; one that cannot run when the
// dynamic closure is unreachable would lock an operator out of the machine
// exactly when they need to diagnose it. `+crt-static` pulls libc.a/libm.a and
// `relocation-model=static` yields a classic ET_EXEC with no PT_INTERP. The
// linker is td's native gcc with `-B` at glibc's crt objects and binutils' as/ld.
//
// The actual static link needs the full target toolchain (no target rustc in
// the loop sandbox); the sibling td-login-test carries that build+assert check.
//
// The crate root (`main.rs`) declares each sibling module with `mod NAME;`, so a
// single `rustc src/main.rs` pulls them all in — but only if every module file is
// present next to it in {src}. MODULES is held to those `mod` lines by
// `the_recipe_writes_out_exactly_the_modules_the_crate_declares` below rather than
// by a comment asking; the crate's own `src_holds_exactly_the_eleven_scanned_modules`
// is the other half of the pin, from the directory side.
//
// Every source below is written out with a WriteFile, which the ladder
// `no_bootstrap_step_invokes_host_find_or_xargs` guard scans as a command
// surface. A `.rs` body is read only INSIDE its string literals, so an
// identifier like `Iterator::find` is free; what must not appear is a bare
// `find`/`xargs` in a LITERAL, which reads exactly as a command name would.
// That guard's roster exempts named reviewed bodies from even that, and none
// of td-login's is on it.
const MAIN_RS: &str = include_str!("../../../td-login/src/main.rs");

// (module basename, source text). rustc resolves `mod NAME;` to `{src}/NAME.rs`.
const MODULES: &[(&str, &str)] = &[
    ("cgroup", include_str!("../../../td-login/src/cgroup.rs")),
    ("creds", include_str!("../../../td-login/src/creds.rs")),
    ("db", include_str!("../../../td-login/src/db.rs")),
    ("exec_as", include_str!("../../../td-login/src/exec_as.rs")),
    ("login", include_str!("../../../td-login/src/login.rs")),
    ("session", include_str!("../../../td-login/src/session.rs")),
    ("status", include_str!("../../../td-login/src/status.rs")),
    ("su", include_str!("../../../td-login/src/su.rs")),
    ("sys", include_str!("../../../td-login/src/sys.rs")),
    ("tty", include_str!("../../../td-login/src/tty.rs")),
];

/// The embedded source of one module, `"main"` included. Lets a consumer that
/// hard-codes a string td-login parses — system-x86-64's health probe spells out
/// `verify-credentials` and its three flags — pin that spelling to the source it
/// came from, instead of the two drifting apart until a boot oracle nobody ran
/// notices.
#[cfg(test)]
pub(crate) fn source(name: &str) -> Option<&'static str> {
    if name == "main" {
        return Some(MAIN_RS);
    }
    MODULES.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
}

/// Every `mod NAME;` the embedded crate root declares. `rustc src/main.rs`
/// resolves each one from the filesystem, so a module `MODULES` does not write
/// out is a compile error — but one that only surfaces in recipe-checks, on a
/// rung that needs the whole target toolchain. Deriving the list from the source
/// makes the mismatch a `cargo test` failure instead.
#[cfg(test)]
fn declared_modules() -> Vec<&'static str> {
    let mut names = Vec::new();
    for line in MAIN_RS.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("mod ") {
            if let Some(name) = rest.strip_suffix(';') {
                names.push(name);
            }
        }
    }
    names
}

pub fn recipe() -> Recipe {
    // The self-hosted toolchains install under a nested stage/td/store/<pkg>
    // DESTDIR (re the /td/store prefix); rust-toolchain installs flat.
    let rustc = "{in:rust-toolchain}/bin/rustc";
    let gcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let gccbin = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin";
    let bbin = "{in:binutils-x86-64-self}/bin";
    let glib = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64/lib";
    // gcc-x86-64-self folds the unwinder objects INTO libgcc.a and never emits a
    // separate static libgcc_eh.a (it built libgcc PIC/shared for rustc's shared
    // driver). A `-static` rustc link still passes `-lgcc_eh` (prebuilt libstd
    // references `_Unwind_*` even under panic=abort), so ld reds "cannot find
    // -lgcc_eh". Synthesize one from libgcc.a (which DOES define `_Unwind_Resume`
    // et al.) into {root}/eh and add it to the link search path — the standard
    // libgcc.a→libgcc_eh.a workaround for a toolchain missing the split EH archive.
    let objcopy = "{in:binutils-x86-64-self}/bin/objcopy";
    let ranlib = "{in:binutils-x86-64-self}/bin/ranlib";
    let libgcc_a = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/lib/gcc/x86_64-pc-linux-gnu/14.3.0/libgcc.a";

    // Bound so they outlive the argv slice; `&String` deref-coerces to `&str`.
    let linker = format!("-Clinker={gcc}");
    let lib_b = format!("-Clink-arg=-B{glib}");
    let bin_b = format!("-Clink-arg=-B{bbin}");
    let path = format!("{bbin}:{gccbin}");

    let mut steps = Vec::new();
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(Step::WriteFile {
        path: "{src}/main.rs".into(),
        content: MAIN_RS.into(),
        exec: false,
    });
    // Every module `main.rs` declares must sit beside it so `rustc src/main.rs`
    // can resolve `mod NAME;` from the filesystem.
    for (name, source) in MODULES {
        steps.push(Step::WriteFile {
            path: format!("{{src}}/{name}.rs"),
            content: (*source).into(),
            exec: false,
        });
    }
    // Synthesize {root}/eh/libgcc_eh.a = libgcc.a (objcopy preserves the members;
    // ranlib writes the archive index ld needs) so `-lgcc_eh` resolves.
    steps.push(Step::MkDir {
        path: "{root}/eh".into(),
    });
    steps.push(Step::run("{root}", &[objcopy, libgcc_a, "{root}/eh/libgcc_eh.a"]).env("PATH", &path));
    steps.push(Step::run("{root}", &[ranlib, "{root}/eh/libgcc_eh.a"]).env("PATH", &path));
    steps.push(
        target_rustc(
            "{src}",
            rustc,
            &[
                "--edition",
                "2021",
                "-C",
                "opt-level=s",
                "--target",
                "x86_64-unknown-linux-gnu",
                "-C",
                "target-feature=+crt-static",
                "-C",
                "relocation-model=static",
                // Mirror the crate's [profile.release] (cargo never sees this
                // direct rustc build): abort — not unwind — on panic. The
                // shared target policy deliberately preserves symbols.
                "-C",
                "panic=abort",
                &linker,
                "-L",
                glib,
                &lib_b,
                &bin_b,
                // The synthesized libgcc_eh.a lives here (see above).
                "-Clink-arg=-L{root}/eh",
                "-Clink-arg=-static-libgcc",
                "-o",
                "{out}/bin/td-login",
                "{src}/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-login".into()],
        exec: true,
    });
    // Fail closed on any interpreter/needed/rpath: a login that dies with the
    // dynamic closure locks the operator out exactly when the closure is what
    // broke.
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-login"]));

    Recipe::mesboot("td-login", "0.1")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
        ])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recipe writes out exactly the modules the crate root declares.
    ///
    /// A `mod` line with no matching `MODULES` entry is a rustc error deep in the
    /// recipe-checks tier, on the one rung that needs the whole target toolchain; an
    /// entry with no `mod` line is a file rustc never compiles, so its contents —
    /// including anything the crate's own confinement tests would have refused —
    /// silently do not reach the shipped binary. Both read as "keep these in
    /// sync" in a comment, which is what this replaces.
    #[test]
    fn the_recipe_writes_out_exactly_the_modules_the_crate_declares() {
        let mut declared = declared_modules();
        let mut written: Vec<&str> = MODULES.iter().map(|(name, _)| *name).collect();
        declared.sort_unstable();
        written.sort_unstable();
        assert_eq!(
            written, declared,
            "MODULES and src/main.rs's `mod` lines disagree; rustc resolves each \
             `mod NAME;` to {{src}}/NAME.rs, so every declared module must be written out \
             and nothing else should be"
        );
        assert!(
            declared.len() >= 8,
            "only {} modules parsed out of the embedded crate root — the scan has gone \
             stale and this test is now vacuous",
            declared.len()
        );
    }

    /// The embedded sources are the SAME bytes the lintable crate carries, which
    /// is what `include_str!` buys and what this asserts is still true of the
    /// accessor consumers reach them through.
    #[test]
    fn every_module_source_is_reachable_by_name() {
        assert!(source("main").is_some_and(|s| s.contains("mod creds;")));
        for name in declared_modules() {
            assert!(
                source(name).is_some_and(|s| !s.is_empty()),
                "no embedded source for module `{name}`"
            );
        }
        assert!(source("nosuch").is_none());
    }

    #[test]
    fn session_cgroup_paths_match_the_distribution_hierarchy() {
        let cgroup = source("cgroup").expect("cgroup source");
        assert!(cgroup.contains(&format!(
            "const SESSION_PROCS: &str = {:?};",
            format!(
                "{}/cgroup.procs",
                crate::ladder::TD_APPLICATION_CGROUP_SESSION
            )
        )));
        assert!(cgroup.contains(&format!(
            "const SESSION_MEMBERSHIP: &str = {:?};",
            format!(
                "0::{}/session",
                crate::ladder::TD_APPLICATION_CGROUP_MEMBERSHIP_ROOT
            )
        )));
    }
}
