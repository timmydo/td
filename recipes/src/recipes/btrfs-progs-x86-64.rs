use crate::ladder::{
    post_bootstrap_path, split_target_debug, unpack_into, unpack_keep_top, POST_BOOTSTRAP_SH,
};
use crate::types::{Recipe, Step};

// A static, target-built Btrfs image writer and offline verifier. Compression
// backends other than the required zlib and unrelated integrations are disabled.
//
// mkfs.btrfs ships in the system image (the live profile formats its volatile
// volume with it), so the whole static closure is compiled by the self-hosted
// toolchain under the shipped target profile and each binary is split into a
// runtime and a debug companion (td-profiler/DESIGN.md §2). Its zlib and
// util-linux-libs archives are built the same way. The one hand-written
// assembly it links, crypto/crc32c-pcl-intel-asm_64.S, is a leaf that never
// touches rsp or rbp, so it keeps every caller's frame chain and needs no
// ASSEMBLY_EXCEPTIONS entry.
/// Busybox applets configure and the Makefile call by name.
const TOOLS: [&str; 45] = [
    "awk", "basename", "cat", "chmod", "cmp", "cp", "cut", "date", "diff", "dirname", "echo",
    "egrep", "env", "expr", "false", "fgrep", "find", "grep", "head", "install", "ln", "ls",
    "mkdir", "mktemp", "mv", "od", "printf", "pwd", "readlink", "rm", "rmdir", "sed", "sleep",
    "sort", "tail", "tee", "test", "touch", "tr", "true", "uname", "uniq", "wc", "which", "xargs",
];

pub fn recipe() -> Recipe {
    let sgcc = "{in:gcc-x86-64-self}/stage/td/store/gcc-14.3.0-x86_64-self/bin/gcc";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let sbin = "{in:binutils-x86-64-self}/bin";
    let ul = "{in:util-linux-libs-x86-64}";
    let zlib = "{in:zlib-x86-64-self}";
    let path = format!(
        "{{root}}/wb:{{tools}}:{{in:make-x86-64-self}}/bin:{sbin}:{}",
        post_bootstrap_path()
    );
    let cip = format!("{ul}/include:{zlib}/include:{xglibc}/include:{{root}}/kh");

    let mut steps = unpack_into("btrfs-progs-x86-64-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(Step::ToolFarm {
        links: TOOLS
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
            "#!{POST_BOOTSTRAP_SH}\nexec \"{sgcc}\" -static -B\"{sbin}/\" -B{xglibc}/lib -L{xglibc}/lib \
             -L{ul}/lib -L{zlib}/lib \"$@\" \
             -fno-omit-frame-pointer -g1 \
             -ffile-prefix-map={{root}}=/td-build-root \
             -ffile-prefix-map={{src}}=/td-build \
             -Wl,--build-id=sha1\n"
        ),
        exec: true,
    });
    // btrfs-progs' release configure script insists on pkg-config even when
    // all three required static libraries are explicit recipe inputs.
    steps.push(Step::WriteFile {
        path: "{root}/wb/pkg-config".into(),
        content: format!(
            "#!{POST_BOOTSTRAP_SH}\n\
             mod=''; cflags=0; libs=0; version=0\n\
             for a in \"$@\"; do\n\
             \tcase \"$a\" in\n\
             \t--atleast-pkgconfig-version) exit 0;;\n\
             \t--cflags) cflags=1;;\n\
             \t--libs) libs=1;;\n\
             \t--modversion) version=1;;\n\
             \t-*) ;;\n\
             \t*) [ -n \"$mod\" ] || mod=\"${{a%% *}}\";;\n\
             \tesac\n\
             done\n\
             case \"$mod\" in\n\
             \tblkid) inc='-I{ul}/include'; link='-L{ul}/lib -lblkid'; ver=2.42.2;;\n\
             \tuuid) inc='-I{ul}/include'; link='-L{ul}/lib -luuid -lpthread'; ver=2.42.2;;\n\
             \tzlib) inc='-I{zlib}/include'; link='-L{zlib}/lib -lz'; ver=1.3.1;;\n\
             \t*) exit 1;;\n\
             esac\n\
             out=''\n\
             [ \"$version\" = 1 ] && out=\"$ver\"\n\
             [ \"$cflags\" = 1 ] && out=\"${{out:+$out }}$inc\"\n\
             [ \"$libs\" = 1 ] && out=\"${{out:+$out }}$link\"\n\
             [ -n \"$out\" ] && printf '%s\\n' \"$out\"\n\
             exit 0\n"
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
                "--prefix=/td/store/btrfs-progs-7.0-x86_64",
                "--disable-backtrace",
                "--disable-documentation",
                "--disable-convert",
                "--disable-zoned",
                "--disable-zstd",
                "--disable-lzo",
                "--disable-libudev",
                "--disable-python",
                "--with-crypto=builtin",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", POST_BOOTSTRAP_SH)
        .env("SHELL", POST_BOOTSTRAP_SH)
        .env("CC", "{root}/wb/cc")
        .env("CC_FOR_BUILD", "{root}/wb/cc")
        .env("AR", "{in:binutils-x86-64-self}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-self}/bin/ranlib")
        .env("PKG_CONFIG", "{root}/wb/pkg-config")
        .env("C_INCLUDE_PATH", &cip)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-x86-64-self}/bin/make",
                "-j{jobs}",
                "mkfs.btrfs.static",
                "btrfs.static",
                &format!("SHELL={POST_BOOTSTRAP_SH}"),
                &format!("CONFIG_SHELL={POST_BOOTSTRAP_SH}"),
            ],
        )
        .env("PATH", &path)
        .env("CC", "{root}/wb/cc")
        .env("AR", "{in:binutils-x86-64-self}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-self}/bin/ranlib")
        .env("C_INCLUDE_PATH", &cip)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::MkDir {
        path: "{out}/bin".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec![
            "{src}/mkfs.btrfs.static".into(),
            "{src}/btrfs.static".into(),
        ],
        dest: "{out}/bin".into(),
    });
    steps.push(Step::Symlink {
        target: "mkfs.btrfs.static".into(),
        link: "{out}/bin/mkfs.btrfs".into(),
    });
    steps.push(Step::Symlink {
        target: "btrfs.static".into(),
        link: "{out}/bin/btrfs".into(),
    });
    steps.push(Step::Require {
        paths: vec!["{out}/bin/mkfs.btrfs".into(), "{out}/bin/btrfs".into()],
        exec: true,
    });
    steps.push(split_target_debug("{out}"));
    steps.push(Step::assert_static(&[
        "{out}/bin/mkfs.btrfs",
        "{out}/bin/btrfs",
    ]));
    steps.push(
        Step::run(
            "{out}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                "for p in bin/mkfs.btrfs bin/btrfs; do \
                   h=$('{in:binutils-x86-64-self}/bin/readelf' -h \"$p\"); \
                   printf '%s\\n' \"$h\" | grep -i 'class:' | grep -qi 'ELF64' || { echo \"$p is not ELF64\" >&2; exit 1; }; \
                   printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || { echo \"$p is not x86-64\" >&2; exit 1; }; \
                 done; \
                 bin/mkfs.btrfs --version | grep -q 'btrfs-progs v7[.]0' || { echo 'mkfs.btrfs version mismatch' >&2; exit 1; }; \
                 bin/btrfs --version | grep -q 'btrfs-progs v7[.]0' || { echo 'btrfs version mismatch' >&2; exit 1; }",
            ],
        )
        .env("PATH", &path),
    );

    Recipe::mesboot("btrfs-progs-x86-64", "7.0")
        .source_input("btrfs-progs-x86-64-source")
        .native_inputs(&[
            "util-linux-libs-x86-64",
            "zlib-x86-64-self",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "make-x86-64-self",
            "busybox-x86-64",
        ])
        .inputs(&["linux-headers-x86-64"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    use super::recipe;
    use crate::types::Step;

    #[test]
    fn shipped_binaries_follow_the_target_profile_and_are_split() {
        let recipe = recipe();
        let inputs = recipe.native_inputs.clone().unwrap_or_default();
        for required in ["gcc-x86-64-self", "binutils-x86-64-self"] {
            assert!(inputs.iter().any(|input| input == required), "{required}");
        }
        for bootstrap in ["gcc-x86-64-native", "binutils-x86-64-native"] {
            assert!(
                !inputs.iter().any(|input| input == bootstrap),
                "{bootstrap}"
            );
        }
        let steps = recipe.steps.unwrap_or_default();
        let wrapper = steps
            .iter()
            .find_map(|step| match step {
                Step::WriteFile { path, content, .. } if path == "{root}/wb/cc" => Some(content),
                _ => None,
            })
            .expect("compiler wrapper");
        let package = wrapper.find("\"$@\"").expect("package flags");
        let policy = wrapper
            .find("-fno-omit-frame-pointer")
            .expect("target profile");
        assert!(package < policy, "package flags could override the profile");
        for required in [
            "-g1",
            "-ffile-prefix-map={root}=/td-build-root",
            "-ffile-prefix-map={src}=/td-build",
            "-Wl,--build-id=sha1",
            "-B\"{in:binutils-x86-64-self}/bin/\"",
        ] {
            assert!(wrapper.contains(required), "missing {required}");
        }
        let split = steps
            .iter()
            .position(|step| matches!(step, Step::SplitDebugTree { root, .. } if root == "{out}"))
            .expect("debug split");
        let installed = steps
            .iter()
            .position(|step| matches!(step, Step::CopyFiles { dest, .. } if dest == "{out}/bin"))
            .expect("install");
        assert!(
            installed < split,
            "the split must see the installed binaries"
        );
        // The static and version checks run on the stripped runtime.
        let checked = steps
            .iter()
            .position(|step| matches!(step, Step::AssertStatic { .. }))
            .expect("static check");
        assert!(split < checked, "the checks must see the shipped runtime");
        assert!(inputs.iter().any(|input| input == "zlib-x86-64-self"));
        assert!(!recipe
            .inputs
            .unwrap_or_default()
            .iter()
            .any(|input| input.starts_with("zlib")));
    }
}
