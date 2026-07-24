use crate::ladder::{mesboot0_inputs, mesboot0_path, unpack_into, unpack_keep_top, SH};
use crate::types::{Recipe, Step};

// A static, target-built Btrfs image writer and offline verifier. Compression
// backends other than the required zlib and unrelated integrations are disabled.
pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let nbin = "{in:binutils-x86-64-native}/bin";
    let ul = "{in:util-linux-libs-x86-64}";
    let zstage = "{root}/zstage";
    let path = format!(
        "{{root}}/wb:{{in:make-x86-64}}/bin:{nbin}:{}",
        mesboot0_path()
    );
    let cip = format!("{ul}/include:{zstage}/include:{xglibc}/include:{{root}}/kh");

    let mut steps = unpack_into("btrfs-progs-x86-64-source", "{src}");
    steps.extend(unpack_into("zlib-x86-64-source", "{root}/zsrc"));
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{SH}\nexec \"{ngcc}\" -static -B{xglibc}/lib -L{xglibc}/lib -L{ul}/lib -L{zstage}/lib \"$@\"\n"
        ),
        exec: true,
    });
    steps.push(Step::ToolFarm {
        links: ["date", "find"]
            .iter()
            .map(|name| ((*name).into(), "{in:busybox-x86-64}/bin/busybox".into()))
            .collect(),
    });

    steps.push(Step::MkDir {
        path: format!("{zstage}/lib"),
    });
    steps.push(Step::MkDir {
        path: format!("{zstage}/include"),
    });
    steps.push(
        Step::run(
            "{root}/zsrc",
            &[SH, "./configure", &format!("--prefix={zstage}"), "--static"],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("AR", "{in:binutils-x86-64-native}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-native}/bin/ranlib")
        .env("C_INCLUDE_PATH", &cip)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{root}/zsrc",
            &[
                "{in:make-x86-64}/bin/make",
                "-j{jobs}",
                "libz.a",
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
    steps.push(Step::CopyFiles {
        files: vec!["{root}/zsrc/libz.a".into()],
        dest: format!("{zstage}/lib"),
    });
    steps.push(Step::CopyFiles {
        files: vec!["{root}/zsrc/zlib.h".into(), "{root}/zsrc/zconf.h".into()],
        dest: format!("{zstage}/include"),
    });

    // btrfs-progs' release configure script insists on pkg-config even when
    // all three required static libraries are explicit recipe inputs.
    steps.push(Step::WriteFile {
        path: "{root}/wb/pkg-config".into(),
        content: format!(
            "#!{SH}\n\
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
             \tzlib) inc='-I{zstage}/include'; link='-L{zstage}/lib -lz'; ver=1.3.1;;\n\
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
                SH,
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
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("CC_FOR_BUILD", "{root}/wb/cc")
        .env("AR", "{in:binutils-x86-64-native}/bin/ar")
        .env("RANLIB", "{in:binutils-x86-64-native}/bin/ranlib")
        .env("PKG_CONFIG", "{root}/wb/pkg-config")
        .env("C_INCLUDE_PATH", &cip)
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make-x86-64}/bin/make",
                "-j{jobs}",
                "mkfs.btrfs.static",
                "btrfs.static",
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
    steps.push(Step::assert_static(&[
        "{out}/bin/mkfs.btrfs",
        "{out}/bin/btrfs",
    ]));
    steps.push(
        Step::run(
            "{out}",
            &[
                SH,
                "-c",
                "for p in bin/mkfs.btrfs bin/btrfs; do \
                   h=$('{in:binutils-x86-64-native}/bin/readelf' -h \"$p\"); \
                   printf '%s\\n' \"$h\" | grep -i 'class:' | grep -qi 'ELF64' || { echo \"$p is not ELF64\" >&2; exit 1; }; \
                   printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || { echo \"$p is not x86-64\" >&2; exit 1; }; \
                 done; \
                 bin/mkfs.btrfs --version | grep -q 'btrfs-progs v7[.]0' || { echo 'mkfs.btrfs version mismatch' >&2; exit 1; }; \
                 bin/btrfs --version | grep -q 'btrfs-progs v7[.]0' || { echo 'btrfs version mismatch' >&2; exit 1; }",
            ],
        )
        .env("PATH", &mesboot0_path()),
    );

    Recipe::mesboot("btrfs-progs-x86-64", "7.0")
        .source_input("btrfs-progs-x86-64-source")
        .native_inputs(&[
            "util-linux-libs-x86-64",
            "gcc-x86-64-native",
            "binutils-x86-64-native",
            "glibc-x86-64",
            "make-x86-64",
            "busybox-x86-64",
        ])
        .inputs_owned(mesboot0_inputs(&[
            "zlib-x86-64-source",
            "linux-headers-x86-64",
        ]))
        .steps(steps)
}
