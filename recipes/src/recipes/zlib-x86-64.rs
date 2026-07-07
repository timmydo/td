use crate::ladder::{base_inputs, base_path, unpack_into, SH};
use crate::types::{Recipe, Step};

// zlib 1.3.1 for x86_64 (#410): the shared libz.so.1 the /td/store Rust toolchain needs
// at RUN time — the upstream rustc's libLLVM.so dynamically NEEDs libz.so.1 (LLVM
// compresses with zlib), but the /td/store x86_64 toolchain (glibc 2.41 + cross gcc
// 14.3.0) ships no zlib. Built FROM SOURCE by the cross gcc 14.3.0 vs the /td/store x86_64
// glibc 2.41 (from-source, not a guix package, not a binary → no guix bytes), so
// `rust-toolchain` can co-locate a td-built libz.so.1 instead of a gate-interned one.
//
// This is the recipe port of the shell `build_zlib_x86_64` that lived in the gate-416
// script (tests/rust-x86_64-runtime-store-native.sh), retired with #410: CC is a wrapper around the cross gcc
// (`{in:gcc-x86-64-stage2}/…/x86_64-pc-linux-gnu-gcc`) that adds the x86_64 glibc 2.41
// headers/libs (`-isystem`/`-B`/`-L`); the glibc-x86-64 output's include already carries the
// kernel UAPI headers glibc's bits/local_lim.h needs, so no separate kernel-header overlay is
// required (the shell path needed one only because it built against the header-less FETCHED
// glibc closure). AR/RANLIB are the cross binutils. Produces
// {out}/stage/td/store/zlib-1.3.1/lib/libz.so.1.3.1 + the libz.so.1 soname link — exactly the
// soname `rust-toolchain`'s transform co-locates. native_inputs: gcc-x86-64-stage2 (the cross
// CC + its libgcc), glibc-x86-64 (headers+libs), binutils-x86-64 (ar/ranlib + the as/ld the
// cross gcc's baked --with-as/--with-ld resolve to).
pub fn recipe() -> Recipe {
    let xgcc = "{in:gcc-x86-64-stage2}/stage/td/store/gcc-14.3.0-x86_64/bin/x86_64-pc-linux-gnu-gcc";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let xar = "{in:binutils-x86-64}/bin/x86_64-pc-linux-gnu-ar";
    let xranlib = "{in:binutils-x86-64}/bin/x86_64-pc-linux-gnu-ranlib";
    let lib = "{out}/stage/td/store/zlib-1.3.1/lib";
    let mut steps = unpack_into("zlib-x86-64-source", "{src}");
    // the cross-gcc wrapper: x86_64 glibc 2.41 headers/libs (crt*.o live in glibc's lib, so -B
    // finds them). The cross gcc self-locates its own libgcc + baked --with-as/--with-ld.
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!(
            "#!{SH}\nexec \"{xgcc}\" -isystem \"{xglibc}/include\" -B\"{xglibc}/lib\" -L\"{xglibc}/lib\" \"$@\"\n"
        ),
        exec: true,
    });
    // zlib's hand-written ./configure honours CC/AR/RANLIB from the env; --shared builds the
    // SHAREDLIBV (libz.so.1.3.1). CHOST names the x86_64 target (AR/RANLIB overridden anyway).
    steps.push(
        Step::run(
            "{src}",
            &[SH, "./configure", "--prefix=/td/store/zlib-1.3.1", "--shared"],
        )
        .env("PATH", &base_path())
        .env("CC", "{root}/wb/cc")
        .env("CHOST", "x86_64-pc-linux-gnu")
        .env("AR", xar)
        .env("RANLIB", xranlib)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH),
    );
    // build ONLY the shared lib target (the transform needs libz.so.1; matches the shell's
    // explicit `make … libz.so.1.3.1`, avoiding the example/minigzip run-programs).
    steps.push(
        Step::run(
            "{src}",
            &["{in:make}/bin/make", "-j{jobs}", "libz.so.1.3.1", &format!("SHELL={SH}"), &format!("CONFIG_SHELL={SH}")],
        )
        .env("PATH", &base_path())
        .env("CC", "{root}/wb/cc")
        .env("AR", xar)
        .env("RANLIB", xranlib),
    );
    // stage libz.so.1.3.1 + the libz.so.1 soname link into the /td/store prefix tree.
    steps.push(Step::MkDir { path: lib.into() });
    steps.push(Step::CopyFiles {
        files: vec!["{src}/libz.so.1.3.1".into()],
        dest: lib.into(),
    });
    steps.push(Step::Symlink {
        target: "libz.so.1.3.1".into(),
        link: format!("{lib}/libz.so.1"),
    });
    steps.push(Step::Require {
        paths: vec![format!("{lib}/libz.so.1.3.1"), format!("{lib}/libz.so.1")],
        exec: false,
    });
    Recipe::mesboot("zlib-x86-64", "1.3.1")
        .source_input("zlib-x86-64-source")
        .native_inputs(&["gcc-x86-64-stage2", "glibc-x86-64", "binutils-x86-64"])
        .inputs_owned(base_inputs(&["make"]))
        .steps(steps)
}
