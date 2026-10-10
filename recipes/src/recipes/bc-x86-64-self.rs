use crate::ladder::{
    post_rust_inputs, post_rust_tool_farm, split_target_debug, unpack_into, unpack_keep_top,
    POST_RUST_SH,
};
use crate::types::{Recipe, Step};

// GNU bc for the kernel build: build-only, as reviewed for retiring BusyBox,
// whose bc applet linux-x86-64 ran for kernel/time/timeconst.bc. That script
// uses GNU bc's print, read() and halt and arithmetic past 64 bits, so
// neither td-util nor awk can stand in. Built on the post-Rust farm
// with the post-Rust gawk and make. The tarball ships bc.c, scan.c and
// libmath.h already generated, so neither lex nor yacc runs; only the library
// and the bc binary are built, not dc or the documentation. Static, without
// readline or libedit.
pub fn recipe() -> Recipe {
    let gcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let binutils = "{in:binutils-x86-64-self}/bin";
    let glibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let make = "{in:make-x86-64-self}/bin/make";
    let path = format!("{{root}}/wb:{{tools}}:{binutils}");

    let mut steps = unpack_into("bc-x86-64-self-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(post_rust_tool_farm("{in:gawk-x86-64-self}/bin/gawk"));
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
                "--without-readline",
                "--without-libedit",
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
    for target in [&["-C", "lib"][..], &["-C", "bc", "bc"][..]] {
        let mut argv = vec![make, "-j{jobs}"];
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
        files: vec!["{src}/bc/bc".into()],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/bc".into()],
        exec: true,
    });
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&["{out}/bin/bc"]));
    // Arithmetic past 64 bits, which timeconst.bc needs, and the shipped,
    // pre-generated libmath.h intact: a broken one would fail to define a(),
    // the arctangent. bc-x86-64-self-test runs a timeconst-shaped program.
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_RUST_SH,
                "-c",
                "r=$(printf '%s\\n' '2^64/3' | '{out}/bin/bc' -q) || exit 1; \
                 [ \"$r\" = 6148914691236517205 ] || { echo \"bc: 2^64/3 gave '$r'\" >&2; exit 1; }; \
                 r=$(printf '%s\\n' 'scale=10; 4*a(1)' | '{out}/bin/bc' -q -l) || exit 1; \
                 [ \"$r\" = 3.1415926532 ] || { echo \"bc -l: 4*a(1) gave '$r'\" >&2; exit 1; }",
            ],
        )
        .env("PATH", "{tools}"),
    );

    Recipe::mesboot("bc-x86-64-self", "1.08.2")
        .source_input("bc-x86-64-self-source")
        .native_inputs(&post_rust_inputs(
            "gawk-x86-64-self",
            &[
                "gcc-x86-64-self",
                "binutils-x86-64-self",
                "glibc-x86-64",
                "make-x86-64-self",
            ],
        ))
        .inputs(&["linux-headers-x86-64"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::recipe;

    #[test]
    fn builds_on_the_post_rust_farm_without_busybox() {
        let recipe = recipe();
        let inputs = recipe.native_inputs.clone().unwrap_or_default();
        for input in ["td-sh", "gawk-x86-64-self", "make-x86-64-self"] {
            assert!(inputs.iter().any(|i| i == input), "missing input {input}");
        }
        assert!(!inputs.iter().any(|i| i == "busybox-x86-64"));
    }
}
