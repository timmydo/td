use crate::ladder::{post_bootstrap_path, unpack_into, unpack_keep_top, POST_BOOTSTRAP_SH};
use crate::types::{Recipe, Step};

// The only util-linux surface btrfs-progs needs: static libuuid and libblkid.
// All programs and unrelated libraries are disabled, and only the two archives
// plus their public headers leave the derivation.
//
// Built past the bootstrap boundary, by the self-hosted toolchain under the
// shipped target profile (td-profiler/DESIGN.md §2): its objects are linked
// into mkfs.btrfs, which ships, and one frame-pointer-less caller truncates
// every stack above it.
/// Busybox applets configure, libtool and the Makefiles call by name.
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
    let path = format!(
        "{{root}}/wb:{{tools}}:{{in:make-x86-64-self}}/bin:{sbin}:{}",
        post_bootstrap_path()
    );
    let cip = format!("{xglibc}/include:{{root}}/kh");

    let mut steps = unpack_into("util-linux-libs-x86-64-source", "{src}");
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
            "#!{POST_BOOTSTRAP_SH}\nexec \"{sgcc}\" -static -B\"{sbin}/\" -B{xglibc}/lib -L{xglibc}/lib \"$@\" \
             -fno-omit-frame-pointer -g1 \
             -ffile-prefix-map={{root}}=/td-build-root \
             -ffile-prefix-map={{src}}=/td-build\n"
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
                "--prefix=/td/store/util-linux-libs-2.42.2-x86_64",
                "--disable-shared",
                "--enable-static",
                "--disable-all-programs",
                "--enable-libuuid",
                "--enable-libblkid",
                "--disable-liblastlog2",
                "--disable-pam-lastlog2",
                "--disable-libmount",
                "--disable-libsmartcols",
                "--disable-libfdisk",
                "--disable-nls",
                "--disable-asciidoc",
                "--disable-poman",
                "--disable-symvers",
                "--without-util",
                "--without-udev",
                "--without-ncursesw",
                "--without-tinfo",
                "--without-readline",
                "--without-cap-ng",
                "--without-libz",
                "--without-libmagic",
                "--without-user",
                "--without-btrfs",
                "--without-systemd",
                "--without-econf",
                "--without-python",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", POST_BOOTSTRAP_SH)
        .env("SHELL", POST_BOOTSTRAP_SH)
        .env("CC", "{root}/wb/cc")
        .env("CC_FOR_BUILD", "{root}/wb/cc")
        .env("AR", "{in:binutils-x86-64-self}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-self}/bin/ranlib")
        .env("C_INCLUDE_PATH", &cip)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-x86-64-self}/bin/make",
                "-j{jobs}",
                "libuuid.la",
                "libblkid.la",
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
        path: "{out}/lib".into(),
    });
    steps.push(Step::MkDir {
        path: "{out}/include/uuid".into(),
    });
    steps.push(Step::MkDir {
        path: "{out}/include/blkid".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec![
            "{src}/.libs/libuuid.a".into(),
            "{src}/.libs/libblkid.a".into(),
        ],
        dest: "{out}/lib".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/libuuid/src/uuid.h".into()],
        dest: "{out}/include/uuid".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/libblkid/src/blkid.h".into()],
        dest: "{out}/include/blkid".into(),
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/lib/libuuid.a".into(),
            "{out}/lib/libblkid.a".into(),
            "{out}/include/uuid/uuid.h".into(),
            "{out}/include/blkid/blkid.h".into(),
        ],
        exec: false,
    });
    steps.push(Step::MkDir {
        path: "{root}/archcheck".into(),
    });
    steps.push(
        Step::run(
            "{root}/archcheck",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                "'{in:binutils-x86-64-self}/bin/ar' x '{out}/lib/libblkid.a'; \
                 o=$(ls *.o 2>/dev/null | head -n1); \
                 [ -n \"$o\" ] || { echo 'libblkid.a contains no objects' >&2; exit 1; }; \
                 h=$('{in:binutils-x86-64-self}/bin/readelf' -h \"$o\"); \
                 printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || { echo 'libblkid.a objects are not x86-64' >&2; exit 1; }",
            ],
        )
        .env("PATH", &path),
    );

    Recipe::mesboot("util-linux-libs-x86-64", "2.42.2")
        .source_input("util-linux-libs-x86-64-source")
        .native_inputs(&[
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
    fn archives_are_compiled_under_the_shipped_target_profile() {
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
        let wrapper = recipe
            .steps
            .unwrap_or_default()
            .into_iter()
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
        ] {
            assert!(wrapper.contains(required), "missing {required}");
        }
    }
}
