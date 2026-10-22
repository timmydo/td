use crate::shared::{
    build_root_static_cc, unpack_into, unpack_keep_top, BUILD_ROOT_PATH, BUILD_ROOT_SH as SH,
};
use crate::types::{Recipe, Step};

// The only util-linux surface cryptsetup needs: static libuuid and libblkid.
// All programs and unrelated libraries are disabled, and only the two archives
// plus their public headers leave the derivation.
//
// Built in the Debian build root against td glibc and gcc-runtime
// (`shared::build_root_static_cc`), under the shipped target profile
// (td-profiler/DESIGN.md §2): its objects are linked into cryptsetup, which
// ships, and one frame-pointer-less caller truncates every stack above it.
pub fn recipe() -> Recipe {
    let path = format!("{{root}}/wb:{BUILD_ROOT_PATH}");
    let cip = "{root}/kh";

    let mut steps = unpack_into("util-linux-libs-x86-64-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.extend(build_root_static_cc());
    steps.push(
        Step::run(
            "{src}",
            &[
                SH,
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
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("CC_FOR_BUILD", "{root}/wb/cc")
        .env("AR", "/usr/bin/ar")
        .env("RANLIB", "/usr/bin/ranlib")
        .env("C_INCLUDE_PATH", cip)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "/usr/bin/make",
                "-j{jobs}",
                "libuuid.la",
                "libblkid.la",
                &format!("SHELL={SH}"),
                &format!("CONFIG_SHELL={SH}"),
            ],
        )
        .env("PATH", &path)
        .env("CC", "{root}/wb/cc")
        .env("AR", "/usr/bin/ar")
        .env("RANLIB", "/usr/bin/ranlib")
        .env("C_INCLUDE_PATH", cip)
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
                SH,
                "-c",
                "'/usr/bin/ar' x '{out}/lib/libblkid.a'; \
                 o=$(ls *.o 2>/dev/null | head -n1); \
                 [ -n \"$o\" ] || { echo 'libblkid.a contains no objects' >&2; exit 1; }; \
                 h=$('/usr/bin/readelf' -h \"$o\"); \
                 printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || { echo 'libblkid.a objects are not x86-64' >&2; exit 1; }",
            ],
        )
        .env("PATH", &path),
    );

    Recipe::mesboot("util-linux-libs-x86-64", "2.42.2")
        .source_input("util-linux-libs-x86-64-source")
        .build_root()
        .native_inputs(&["glibc-x86-64", "gcc-runtime"])
        .inputs(&["linux-headers-x86-64"])
        .steps(steps)
}

#[cfg(test)]
mod tests {
    #[test]
    fn archives_are_compiled_in_the_build_root_under_the_target_profile() {
        crate::shared::assert_build_root_static_c(&super::recipe());
    }
}
