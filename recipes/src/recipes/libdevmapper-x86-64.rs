use crate::ladder::{post_bootstrap_path, unpack_into, unpack_keep_top, POST_BOOTSTRAP_SH};
use crate::types::{Recipe, Step};

// LVM2's device-mapper library alone, for cryptsetup (td-install/
// ENCRYPTION.md increment 3). LVM2 documents a libdm-only build without libaio;
// no LVM tool, daemon, udev rule or synchronization enters the output, only
// libdevmapper.a and libdevmapper.h.
//
// Built by the self-hosted toolchain under the shipped target profile
// (td-profiler/DESIGN.md §2) because its objects are linked into cryptsetup.
/// Busybox applets configure and the Makefiles call by name.
const TOOLS: &[&str] = &[
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

    let mut steps = unpack_into("libdevmapper-x86-64-source", "{src}");
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
    // LVM2's configure requires a pkg-config program even when every module
    // it would query is disabled; this one answers version probes and reports
    // every module absent, so nothing outside the recipe inputs is found.
    steps.push(Step::WriteFile {
        path: "{root}/wb/pkg-config".into(),
        content: format!(
            "#!{POST_BOOTSTRAP_SH}\n\
             for a in \"$@\"; do\n\
             \tcase \"$a\" in\n\
             \t--version) echo 0.29.2; exit 0;;\n\
             \t--atleast-pkgconfig-version) exit 0;;\n\
             \tesac\n\
             done\n\
             exit 1\n"
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
                "--prefix=/td/store/libdevmapper-2.03.43-x86_64",
                "--enable-static_link",
                "--disable-udev_sync",
                "--disable-udev_rules",
                "--disable-selinux",
                "--disable-blkid_wiping",
                "--disable-readline",
                "--disable-nls",
                "--disable-dmeventd",
                "--disable-cmdlib",
                "--disable-pkgconfig",
                "--disable-systemd-journal",
                "--disable-nvme-wwid",
                "--without-libnvme",
                "--without-blkid",
                "--without-udev",
                "--without-systemd",
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
            "{src}/libdm",
            &[
                "{in:make-x86-64-self}/bin/make",
                "-j{jobs}",
                "ioctl/libdevmapper.a",
                &format!("SHELL={POST_BOOTSTRAP_SH}"),
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
        path: "{out}/include".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/libdm/ioctl/libdevmapper.a".into()],
        dest: "{out}/lib".into(),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/libdm/libdevmapper.h".into()],
        dest: "{out}/include".into(),
    });
    steps.push(Step::Require {
        paths: vec![
            "{out}/lib/libdevmapper.a".into(),
            "{out}/include/libdevmapper.h".into(),
        ],
        exec: false,
    });

    Recipe::mesboot("libdevmapper-x86-64", "2.03.43")
        .source_input("libdevmapper-x86-64-source")
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
