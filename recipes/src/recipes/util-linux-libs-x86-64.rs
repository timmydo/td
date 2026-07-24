use crate::ladder::{mesboot0_inputs, mesboot0_path, unpack_into, unpack_keep_top, SH};
use crate::types::{Recipe, Step};

// The only util-linux surface btrfs-progs needs: static libuuid and libblkid.
// All programs and unrelated libraries are disabled, and only the two archives
// plus their public headers leave the derivation.
pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let nbin = "{in:binutils-x86-64-native}/bin";
    let path = format!(
        "{{root}}/wb:{{in:make-x86-64}}/bin:{nbin}:{}",
        mesboot0_path()
    );
    let cip = format!("{xglibc}/include:{{root}}/kh");

    let mut steps = unpack_into("util-linux-libs-x86-64-source", "{src}");
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!("#!{SH}\nexec \"{ngcc}\" -static -B{xglibc}/lib -L{xglibc}/lib \"$@\"\n"),
        exec: true,
    });
    steps.push(Step::ToolFarm {
        links: vec![(
            "find".into(),
            "{in:busybox-x86-64}/bin/busybox".into(),
        )],
    });
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
        .env("AR", "{in:binutils-x86-64-native}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-native}/bin/ranlib")
        .env("C_INCLUDE_PATH", &cip)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-x86-64}/bin/make",
                "-j{jobs}",
                "libuuid.la",
                "libblkid.la",
                "SHELL={in:bash-mesboot}/bin/bash",
                "CONFIG_SHELL={in:bash-mesboot}/bin/bash",
            ],
        )
        .env("PATH", &path)
        .env("CC", "{root}/wb/cc")
        .env("AR", "{in:binutils-x86-64-native}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-native}/bin/ranlib")
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
                SH,
                "-c",
                "'{in:binutils-x86-64-native}/bin/ar' x '{out}/lib/libblkid.a'; \
                 o=$(ls *.o 2>/dev/null | head -n1); \
                 [ -n \"$o\" ] || { echo 'libblkid.a contains no objects' >&2; exit 1; }; \
                 h=$('{in:binutils-x86-64-native}/bin/readelf' -h \"$o\"); \
                 printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || { echo 'libblkid.a objects are not x86-64' >&2; exit 1; }",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    Recipe::mesboot("util-linux-libs-x86-64", "2.42.2")
        .source_input("util-linux-libs-x86-64-source")
        .native_inputs(&[
            "gcc-x86-64-native",
            "binutils-x86-64-native",
            "glibc-x86-64",
            "make-x86-64",
            "busybox-x86-64",
        ])
        .inputs_owned(mesboot0_inputs(&["linux-headers-x86-64"]))
        .steps(steps)
}
