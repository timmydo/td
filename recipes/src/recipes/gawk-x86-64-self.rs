use crate::ladder::{
    post_rust_inputs, post_rust_tool_farm, split_target_debug, unpack_into, unpack_keep_top,
    POST_RUST_SH,
};
use crate::types::{Recipe, Step, TextEdit};

// GNU awk for the builds after rust-toolchain: build-only, as reviewed for
// retiring BusyBox, and the awk every post-Rust tool farm links. It is the
// first build on that farm (td-sh, td-txt, td-util and uutils). Its own
// configure needs an awk and its build a make, and the post-Rust ones would
// themselves be built with this gawk, so it takes the root's gawk-mesboot and
// the bootstrap make-x86-64 once, as make-x86-64-self does. Static, without
// the persistent-memory allocator, dynamic extensions, MPFR or readline: a
// build tool needs none of them. gawk runs system(), pipes and coprocesses
// through execl("/bin/sh"), which no sandbox has, so those name td-sh
// instead. The DEFPATH and DEFLIBPATH it bakes ({out}/share/awk,
// {out}/lib/gawk) are never installed: AWKPATH lookup tries `.` first and
// extensions are off, so a build loses only awklib's @include files.
pub fn recipe() -> Recipe {
    let gcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let binutils = "{in:binutils-x86-64-self}/bin";
    let glibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let path = format!("{{root}}/wb:{{tools}}:{binutils}");

    let mut steps = unpack_into("gawk-x86-64-self-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(post_rust_tool_farm("{in:gawk-mesboot}/bin/gawk"));
    let to_td_sh = |expect| {
        vec![TextEdit::new(
            "execl(\"/bin/sh\", \"sh\", \"-c\", ",
            "execl(TD_SHELL, \"sh\", \"-c\", ",
            expect,
        )]
    };
    steps.push(Step::SubstituteText {
        file: "{src}/builtin.c".into(),
        edits: to_td_sh(1),
    });
    steps.push(Step::SubstituteText {
        file: "{src}/io.c".into(),
        edits: to_td_sh(5),
    });
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: POST_RUST_SH.into(),
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{POST_RUST_SH}\n\
             exec \"{gcc}\" -static -idirafter \"{glibc}/include\" \
             -idirafter \"{{root}}/kh\" -B\"{binutils}/\" \
             -B\"{glibc}/lib\" -L\"{glibc}/lib\" \"$@\" \
             '-DTD_SHELL=\"{POST_RUST_SH}\"' \
             -fno-omit-frame-pointer -g1 \
             -ffile-prefix-map=\"{{root}}\"=/td-build-root \
             -ffile-prefix-map=\"{{src}}\"=/td-build \
             -Wl,--build-id=sha1\n"
        ),
        exec: true,
    });
    steps.push(
        Step::run(
            "{src}",
            &[
                POST_RUST_SH,
                "./configure",
                "--build=x86_64-pc-linux-gnu",
                "--host=x86_64-pc-linux-gnu",
                "--prefix={out}",
                "--disable-dependency-tracking",
                "--disable-nls",
                "--disable-pma",
                "--disable-extensions",
                "--without-readline",
                "--without-mpfr",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", POST_RUST_SH)
        .env("SHELL", POST_RUST_SH)
        .env("CC", "{root}/wb/cc")
        .env("AR", "{in:binutils-x86-64-self}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-self}/bin/ranlib")
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    // Only the interpreter: its gnulib archive, then the binary, without the
    // documentation, extensions or test trees the default target recurses into.
    for target in [&["-C", "support"][..], &["gawk"][..]] {
        let mut argv = vec!["{in:make-x86-64}/bin/make", "-j{jobs}"];
        argv.extend_from_slice(target);
        let shell = format!("SHELL={POST_RUST_SH}");
        argv.extend_from_slice(&[shell.as_str(), "MAKEINFO=true"]);
        steps.push(
            Step::run("{src}", &argv)
                .env("PATH", &path)
                .env("CONFIG_SHELL", POST_RUST_SH)
                .env("SHELL", POST_RUST_SH)
                .env("MAKEFLAGS", "")
                .env("MFLAGS", "")
                .env("GNUMAKEFLAGS", "")
                .env("MAKELEVEL", "")
                .env("SOURCE_DATE_EPOCH", "1"),
        );
    }
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/gawk".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Symlink {
        target: "gawk".into(),
        link: "{out}/bin/awk".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/gawk".into()],
        exec: true,
    });
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/gawk"]));
    steps.push(Step::run(
        "{out}",
        &[
            "{out}/bin/gawk",
            "BEGIN { n = split(\"td awk\", w); if (n != 2 || toupper(w[2]) != \"AWK\") exit 1 }",
        ],
    ));

    Recipe::mesboot("gawk-x86-64-self", "5.4.1")
        .source_input("gawk-x86-64-self-source")
        .native_inputs(&post_rust_inputs(
            "gawk-mesboot",
            &[
                "gcc-x86-64-self",
                "binutils-x86-64-self",
                "glibc-x86-64",
                "make-x86-64",
            ],
        ))
        .inputs(&["linux-headers-x86-64"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::recipe;
    use crate::types::Step;

    #[test]
    fn builds_on_the_post_rust_farm_with_the_root_awk_and_bootstrap_make() {
        let recipe = recipe();
        let inputs = recipe.native_inputs.clone().unwrap_or_default();
        for input in [
            "td-sh",
            "td-txt",
            "td-util",
            "uutils",
            "gawk-mesboot",
            "make-x86-64",
        ] {
            assert!(inputs.iter().any(|i| i == input), "missing input {input}");
        }
        assert!(!inputs.iter().any(|i| i == "busybox-x86-64"));
        let steps = recipe.steps.unwrap_or_default();
        let farm = steps.iter().find_map(|step| match step {
            Step::ToolFarm { links } => Some(links),
            _ => None,
        });
        let farm = farm.expect("tool farm");
        assert!(farm
            .iter()
            .any(|(name, target)| name == "awk" && target == "{in:gawk-mesboot}/bin/gawk"));
        assert!(farm
            .iter()
            .any(|(name, target)| name == "sh" && target == "{in:td-sh}/bin/td-sh"));
    }
}
