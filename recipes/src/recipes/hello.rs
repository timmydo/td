use crate::ladder::{mesboot0_inputs, unpack_into, unpack_keep_top, SH};
use crate::types::{Recipe, Step};

// GNU Hello 2.10 — the first ordinary package built by the complete native
// x86_64 recipe graph (#424). Its build userland is explicit: make-x86-64
// drives the upstream configure/make/install flow, while every ordinary tool
// name resolves to the declared BusyBox output through the ToolFarm below.
// bash-mesboot remains the declared configure interpreter; no host /bin, /usr,
// ambient PATH, or host store path is available to a recipe step.
//
// The installed hello is deliberately dynamic. Its interpreter and RUNPATH
// name td's source-built glibc input, proving that the native GCC/binutils/libc
// graph can build a real consumer rather than only rebuilding its own rungs.
// Runtime behavior and the /gnu/store-absent boundary are owned by hello-test.
pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc";
    let nbin = "{in:binutils-x86-64-native}/bin";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let path = format!("{{tools}}:{nbin}");
    let mut steps = unpack_into("hello-source", "{src}");

    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    steps.push(Step::ToolFarm {
        links: [
            "awk", "basename", "cat", "chmod", "cmp", "comm", "cp", "cut", "date", "diff",
            "dirname", "echo", "env", "expr", "false", "grep", "head", "install", "ln", "ls",
            "mkdir", "mktemp", "mv", "printf", "pwd", "rm", "sed", "sleep", "sort", "tail", "tee",
            "touch", "tr", "true", "uname", "wc", "which", "yes",
        ]
        .iter()
        .map(|name| ((*name).into(), "{in:busybox-x86-64}/bin/busybox".into()))
        .chain(std::iter::once((
            "make".into(),
            "{in:make-x86-64}/bin/make".into(),
        )))
        .collect(),
    });
    steps.push(Step::PatchShebangs {
        dir: "{src}".into(),
        shell: SH.into(),
    });
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{SH}\n\
             exec \"{ngcc}\" -isystem \"{xglibc}/include\" -idirafter \"{{root}}/kh\" \
             -B\"{nbin}/\" -B\"{xglibc}/lib\" -L\"{xglibc}/lib\" -static-libgcc \
             -Wl,--dynamic-linker -Wl,\"{xglibc}/lib/ld-linux-x86-64.so.2\" \
             -Wl,--enable-new-dtags -Wl,-rpath -Wl,\"{xglibc}/lib\" \"$@\"\n"
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
                "--prefix={out}",
                "--disable-dependency-tracking",
                "--disable-nls",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}",
            &["{tools}/make", "-j{jobs}", &format!("SHELL={SH}")],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("MAKEFLAGS", "")
        .env("MFLAGS", "")
        .env("GNUMAKEFLAGS", "")
        .env("MAKELEVEL", "")
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(
        Step::run(
            "{src}",
            &["{tools}/make", "install", &format!("SHELL={SH}")],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("MAKEFLAGS", "")
        .env("MFLAGS", "")
        .env("GNUMAKEFLAGS", "")
        .env("MAKELEVEL", "")
        .env("SOURCE_DATE_EPOCH", "1"),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/hello".into()],
        exec: true,
    });

    Recipe::mesboot("hello", "2.10")
        .source_input("hello-source")
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
