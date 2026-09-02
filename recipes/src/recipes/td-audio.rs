use crate::ladder::{split_target_debug, target_rustc};
use crate::types::{Recipe, Step};

// td-audio is compiled directly with the source-built target rustc and no
// third-party crates. It is static for the same reason td-profiler is: a daemon
// that owns the only path to the hardware must not depend on a dynamic loader
// that the thing it is diagnosing may have broken. The crate is the one source
// of truth; this recipe embeds every sibling module beside main.rs for direct
// rustc module resolution.
//
// This is APPLICATIONS.md §I rungs 25 and 26 — the ALSA PCM back end and its
// mixer, driven by a tone fixture, plus the PulseAudio protocol and the socket
// the daemon serves it on. The system image supplies §K.5's dedicated `audio`
// account before its unit selects and starts this output.
const MAIN_RS: &str = include_str!("../../../td-audio/src/main.rs");
const MODULES: &[(&str, &str)] = &[
    ("alsa", include_str!("../../../td-audio/src/alsa.rs")),
    ("device", include_str!("../../../td-audio/src/device.rs")),
    ("mixer", include_str!("../../../td-audio/src/mixer.rs")),
    ("pcm", include_str!("../../../td-audio/src/pcm.rs")),
    ("proto", include_str!("../../../td-audio/src/proto.rs")),
    ("serve", include_str!("../../../td-audio/src/serve.rs")),
    ("session", include_str!("../../../td-audio/src/session.rs")),
    ("sink", include_str!("../../../td-audio/src/sink.rs")),
    ("sys", include_str!("../../../td-audio/src/sys.rs")),
    ("tag", include_str!("../../../td-audio/src/tag.rs")),
    ("tone", include_str!("../../../td-audio/src/tone.rs")),
    ("wav", include_str!("../../../td-audio/src/wav.rs")),
    ("wire", include_str!("../../../td-audio/src/wire.rs")),
];

#[cfg(test)]
fn declared_modules() -> Vec<String> {
    declared_in(MAIN_RS)
}

/// Every module a source text declares.
///
/// Over the WHOLE text rather than line by line: Rust does not care where the
/// newlines are, so `mod` on one line and `extra;` on the next is a declaration
/// that a per-line scan cannot see — which a confirmation pass used to put a
/// module back out of reach after the per-line version was written.
#[cfg(test)]
fn declared_in(text: &str) -> Vec<String> {
    let code: String = text
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    let mut found = Vec::new();
    let mut words = code.split_whitespace().peekable();
    while let Some(word) = words.next() {
        if word != "mod" {
            continue;
        }
        // `mod NAME;` is a declaration; `mod NAME {` is inline and its body is
        // already inside a file this scan reads.
        if let Some(next) = words.peek() {
            if let Some(name) = next.strip_suffix(';') {
                found.push(name.to_string());
            }
        }
    }
    found
}

/// The files this recipe actually stages under `{src}`, with their contents.
#[cfg(test)]
fn staged_files() -> Vec<(String, String)> {
    recipe()
        .steps
        .unwrap_or_default()
        .into_iter()
        .filter_map(|step| match step {
            crate::types::Step::WriteFile { path, content, .. } => path
                .strip_prefix("{src}/")
                .map(|name| (name.to_string(), content)),
            _ => None,
        })
        .collect()
}

pub fn recipe() -> Recipe {
    let rustc = "{in:rust-toolchain}/bin/rustc";
    let gcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let gccbin = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin";
    let bbin = "{in:binutils-x86-64-self}/bin";
    let glib = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64/lib";
    let objcopy = "{in:binutils-x86-64-self}/bin/objcopy";
    let ranlib = "{in:binutils-x86-64-self}/bin/ranlib";
    let libgcc_a = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/lib/gcc/x86_64-pc-linux-gnu/14.3.0/libgcc.a";
    let linker = format!("-Clinker={gcc}");
    let lib_b = format!("-Clink-arg=-B{glib}");
    let bin_b = format!("-Clink-arg=-B{bbin}");
    let path = format!("{bbin}:{gccbin}");

    let mut steps = vec![
        Step::MkDir {
            path: "{out}/bin".into(),
        },
        Step::WriteFile {
            path: "{src}/main.rs".into(),
            content: MAIN_RS.into(),
            exec: false,
        },
    ];
    for (name, source) in MODULES {
        steps.push(Step::WriteFile {
            path: format!("{{src}}/{name}.rs"),
            content: (*source).into(),
            exec: false,
        });
    }
    steps.push(Step::MkDir {
        path: "{root}/eh".into(),
    });
    steps.push(
        Step::run("{root}", &[objcopy, libgcc_a, "{root}/eh/libgcc_eh.a"]).env("PATH", &path),
    );
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
                "-C",
                "panic=abort",
                &linker,
                "-L",
                glib,
                &lib_b,
                &bin_b,
                "-Clink-arg=-L{root}/eh",
                "-Clink-arg=-static-libgcc",
                "-o",
                "{out}/bin/td-audio",
                "{src}/main.rs",
            ],
        )
        .env("PATH", &path)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/td-audio".into()],
        exec: true,
    });
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/td-audio"]));

    Recipe::mesboot("td-audio", "0.1")
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

    /// What the root declares, what this recipe embeds, and what it STAGES are
    /// one set.
    ///
    /// The third is the one that was missing. `MODULES` is what the embedding
    /// loop reads, but a step written beside that loop stages a file the loop
    /// never mentions, and the crate root can reach it with `include!` — which
    /// needs no `mod` and so leaves the declaration list untouched too.
    #[test]
    fn the_recipe_embeds_exactly_the_declared_modules() {
        let mut declared = declared_modules();
        let mut written: Vec<_> = MODULES.iter().map(|(name, _)| name.to_string()).collect();
        declared.sort_unstable();
        written.sort_unstable();
        assert_eq!(written, declared);
        assert_eq!(written.len(), 13);

        let mut staged: Vec<String> = staged_files().into_iter().map(|(name, _)| name).collect();
        let mut expected: Vec<String> = std::iter::once("main.rs".to_string())
            .chain(MODULES.iter().map(|(name, _)| format!("{name}.rs")))
            .collect();
        staged.sort_unstable();
        expected.sort_unstable();
        assert_eq!(
            staged, expected,
            "the staged file set and the embedded module set must be the same"
        );
    }

    #[test]
    fn the_target_binary_is_static_profiled_and_split() {
        let recipe = recipe();
        let steps = recipe.steps.unwrap_or_default();
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::AssertStatic { paths } if paths == &["{out}/bin/td-audio"]
        )));
        assert!(steps.iter().any(|step| matches!(
            step,
            Step::SplitDebugTree { root, .. } if root == "{out}"
        )));
    }

    #[test]
    fn daemon_socket_mode_matches_the_jail_contract() {
        let serve = MODULES
            .iter()
            .find_map(|(name, source)| (*name == "serve").then_some(*source))
            .expect("serve module");
        let expected = format!(
            "pub const SOCKET_MODE: u32 = {:#o};",
            td_engine::permissions::TD_AUDIO_SOCKET_MODE
        );
        assert!(
            serve.contains(&expected),
            "td-audio must publish the mode td-jail validates: {expected}"
        );
    }

    /// The shipped text carries the surface `UNSAFE.md` §13 records, and nothing
    /// more.
    ///
    /// The crate's own confinement tests already hold this, but they hold it for
    /// the crate as host cargo lints it. This holds it for the bytes the recipe
    /// STAGES, which is what actually becomes a target binary — the two are the
    /// same text only because this table says so, and that is the claim worth
    /// checking here.
    #[test]
    fn the_staged_source_carries_only_the_amended_surface() {
        let sys = MODULES
            .iter()
            .find_map(|(name, source)| (*name == "sys").then_some(*source))
            .unwrap_or_default();
        assert!(!sys.is_empty(), "the sys module is not staged");
        assert!(sys.contains("const SYS_IOCTL: usize = 16;"));
        assert!(sys.contains("const SYS_POLL: usize = 7;"));
        assert!(sys.contains("const SYS_GETSOCKOPT: usize = 55;"));
        // Three syscalls, and no fourth: the roster is what `UNSAFE.md` §13
        // counts.
        assert_eq!(sys.matches(concat!("const ", "SYS_")).count(), 3);
        // And the one socket option, with its level, pinned by value.
        assert!(sys.contains("const SOL_SOCKET: usize = 1;"));
        assert!(sys.contains("const SO_PEERCRED: usize = 17;"));
        // One inline-assembly block in the staged text, whatever the module
        // count. Five argument registers is one register mapping, not two.
        assert_eq!(
            MODULES
                .iter()
                .map(|(_, source)| source.matches(concat!("a", "sm!")).count())
                .sum::<usize>(),
            1
        );
        // The crate root denies unsafe, and exactly one item in the whole
        // staged tree relaxes it — the raw entry point, in `sys`. The root
        // carries no allowance of its own: a module-level one would exempt
        // every line of that module, which a review demonstrated by adding a
        // second unsafe block that no test could see.
        assert!(MAIN_RS.contains(concat!("#![deny(un", "safe_code)]")));
        assert_eq!(
            MAIN_RS.matches(concat!("#[allow(un", "safe_code)]")).count(),
            0
        );
        // Derived from the steps this recipe EMITS, not from the module list
        // it happens to build them out of. Iterating `MODULES` scans the list;
        // a reviewer added one more `WriteFile` under `{src}` and an `include!`
        // in the root, and that extra file — carrying its own allowance and a
        // dereference — was staged, compiled and shipped while every assertion
        // here passed, because nothing tied what is scanned to what is staged.
        let files = staged_files();
        assert!(
            files.iter().any(|(name, _)| name == "main.rs"),
            "the crate root is staged"
        );
        let staged: String = files.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(
            staged.matches(concat!("#[allow(un", "safe_code)]")).count(),
            1
        );
        assert!(!staged.contains(concat!("#![allow(un", "safe_code)]")));
        // And every form of the keyword, not only a block. Whitespace is
        // squeezed first: the staged text is real source, so `unsafe {` and
        // `unsafe{` are the same region and only one of them is a literal.
        // Comments are stripped first, and not for tidiness: `sys.rs` and the
        // crate's own confinement tests NAME the refused forms in prose, which
        // is the point of a refusal being recorded. A scan that read its own
        // documentation would make every one of those files a violation.
        let code: String = staged
            .lines()
            .map(|line| line.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        let squeezed: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        for (form, expected) in [("{", 1usize), ("fn", 0), ("impl", 0), ("trait", 0), ("extern", 0)] {
            let uses = squeezed
                .matches(&format!("{}{form}", concat!("un", "safe")))
                .count();
            assert_eq!(
                uses, expected,
                "the staged tree has {uses} `{}{form}`, expected {expected}",
                concat!("un", "safe")
            );
        }
        for (name, source) in MODULES {
            if *name == "sys" {
                continue;
            }
            assert!(
                !source.contains(concat!("un", "safe {")),
                "the staged {name} module contains an unsafe block"
            );
        }
        // THE bound, over the bytes that actually ship. Everything above
        // matches a shape in text with line comments stripped and whitespace
        // squeezed, and a review walked past all of it three ways: a block
        // comment between the keyword and its form (not whitespace, not
        // stripped), a conditional attribute (not the literal), and `//` inside
        // a string literal (blinds the strip for the rest of the line). Each
        // compiled, ran, and passed this scan.
        //
        // Counting the keyword cannot be walked past. Rust has one spelling of
        // it, and text that merely mentions it pushes the count up rather than
        // down — the direction that fails closed. The crate's own tests pin the
        // same numbers; this pins them over the staged tree, which is what a
        // build puts in the image.
        let keyword = concat!("un", "safe");
        for (name, text) in &files {
            let uses = text.matches(keyword).count();
            let expected = match name.as_str() {
                "main.rs" => KEYWORD_USES_IN_MAIN,
                "sys.rs" => KEYWORD_USES_IN_SYS,
                _ => 0,
            };
            assert_eq!(
                uses, expected,
                "the staged {name} names the keyword {uses} times, and the \
                 roster says {expected}"
            );
        }
        // A conditional attribute can spell any other attribute, including the
        // allowance counted above and a `path` that redirects a module out of
        // this list entirely. Nothing here needs one.
        assert!(
            !staged.contains(concat!("#[cfg_", "attr(")),
            "the staged tree writes an attribute conditionally"
        );
        // No module is declared anywhere but the root. One declared in `sys`
        // would resolve to `sys/extra.rs`, which this recipe stages nowhere and
        // no scan here reads — a confirmation pass put a back door there and
        // every assertion passed. The staged tree is flat, so the build would
        // fail on the missing file; failing in the scan says why.
        for (name, text) in &files {
            if name == "main.rs" {
                continue;
            }
            assert!(
                declared_in(text).is_empty(),
                "the staged {name} declares a submodule"
            );
        }
        // And nothing pulls in a file by path. `include!` needs no `mod`, so a
        // file named by neither list is read by the compiler and by no scan.
        assert!(
            !staged.contains(concat!("incl", "ude!")),
            "the staged tree includes a file by path"
        );
        // And no block comments: one splits the keyword from its form, and from
        // an attribute's contents, while still compiling — which is what walked
        // past the shape assertions above.
        assert!(
            !staged.contains(concat!("/", "*")),
            "the staged tree writes a block comment"
        );
    }

    /// The staged `sys` module: the scoped allowance, and the block it scopes.
    ///
    /// Exactly the tokens that must be there and not one more. A pin with slack
    /// in it is a budget an attacker spends — delete a sentence naming the
    /// keyword, add a region that uses it, and the total is unchanged. So no
    /// comment in either file names it, and every added region moves a count.
    const KEYWORD_USES_IN_SYS: usize = 2;
    /// And the staged crate root: the crate-level denial, and nothing else.
    const KEYWORD_USES_IN_MAIN: usize = 1;

    /// The two §K.4 refusals that would be invisible in a passing build: the
    /// mmap machinery, and the card control device.
    #[test]
    fn the_staged_source_refuses_the_mmap_ring_and_the_control_device() {
        for (name, source) in MODULES {
            assert!(
                !source.contains(concat!("SNDRV_PCM_IOCTL_", "SYNC_PTR")),
                "the staged {name} module reaches for the mapped-ring interface"
            );
            assert!(
                !source.contains(concat!("/dev/snd/cont", "rolC")),
                "the staged {name} module names the card control device"
            );
        }
    }
}
