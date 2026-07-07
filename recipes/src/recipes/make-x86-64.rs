use crate::ladder::{base_inputs, base_path, unpack_into, unpack_keep_top, SH};
use crate::types::{Recipe, RecipeCheck, Step};

// GNU Make 4.4.1 — the FIRST td-native build-userland tool, built FROM SOURCE by the
// /td/store NATIVE x86_64 toolchain (issue #388 rung 1, the north-star re-aim: new
// packages build on td packages from the mes-rooted chain, not on guix-built build
// tools). The compiler is the ELF64 x86_64 native gcc 14.3.0 (gcc-x86-64-native) +
// native binutils 2.44 (binutils-x86-64-native) — the "compiler-runs-in-sandbox"
// native track #411 built for exactly this, NOT the i686 CROSS gcc. Linked STATIC
// against the /td/store x86_64 glibc 2.41 (glibc-x86-64): a static make has no ELF
// interp, so it runs in any own-root / build sandbox with NO glibc staging — the
// self-contained shape a hermetic build userland wants (and how guix's own bootstrap
// make + the sibling native toolchain rungs are built). The build DRIVER make is the
// seed host make ({in:make}/bin/make, a `tool:` lock entry) — the new make does not
// build itself; its OUTPUT is the /td/store make. No guix bytes in the output.
//
// STATIC vs dynamic (directive-3-style callout, see PR): seed/sources/make-4.4.1.lock's
// comment describes the loop-driver make as "dynamic vs /td/store glibc 2.41". This rung
// deliberately builds it STATIC — the lowest-risk, fewest-moving-parts proof of "a
// /td/store userland tool built on the native toolchain that RUNS" (a static bin needs no
// interp path co-located at run time, and configure's AC_RUN conftests execute directly
// on the x86_64 host with no loader). A dynamic build (matching rust-toolchain's interp
// relink + glibc co-location) is the alternative if the loop-driver make must be dynamic;
// flagged for the maintainer.
//
// Validation is a recipe-owned RecipeCheck::daily (below): `build-plan --auto make-x86-64`
// builds the tree over the native toolchain, make 4.4.1 RUNS from /td/store in a store-ns
// own-root (/gnu/store ABSENT) AND drives a one-rule build. HEAVY (from-seed native
// toolchain, ~memory-heavy, #371) — deferred to the daily backstop, same posture as the
// rust-toolchain recipe check it is modeled on.
pub fn recipe() -> Recipe {
    let ngcc = "{in:gcc-x86-64-native}/stage/td/store/gcc-14.3.0-x86_64-native/bin/gcc";
    let xglibc = "{in:glibc-x86-64}/stage/td/store/glibc-2.41-x86_64";
    let nbin = "{in:binutils-x86-64-native}/bin";
    let path = format!("{nbin}:{}", base_path());
    // glibc 2.41 headers + the x86_64 kernel UAPI headers (glibc's <sys/…> #include
    // <linux/…>) via C_INCLUDE_PATH — make is pure C, so C_INCLUDE_PATH is safe here
    // (the libstdc++ <cstdlib> #include_next hazard that bars it for gcc/g++ does not
    // apply). Mirrors binutils-x86-64-native.
    let cip = format!("{xglibc}/include:{{root}}/kh");
    let mut steps = unpack_into("make-x86-64-source", "{src}");
    // kernel headers keep-top: the tarball top level is {linux,asm,…} → {root}/kh/*.
    steps.extend(unpack_keep_top("linux-headers-x86-64", "{root}/kh"));
    // STATIC wrapper around the NATIVE gcc: -static (no interp, own-root-runnable), -B at
    // the x86_64 glibc lib for crt*.o + libc.a. The installed native gcc has no baked-in
    // sysroot (only a build-time --with-build-sysroot), so headers/libs are supplied
    // explicitly. Port of binutils-x86-64-native's wrapper with the native gcc swapped in.
    steps.push(Step::WriteFile {
        path: "{root}/wb/cc".into(),
        content: format!("#!{SH}\nexec \"{ngcc}\" -static -B{xglibc}/lib \"$@\"\n"),
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
                "--prefix=/td/store/make-4.4.1-x86_64",
                "--disable-nls",
                // make optionally links Guile for $(guile …); there is no guile input, so
                // build it out explicitly (deterministic closure = glibc only).
                "--without-guile",
            ],
        )
        .env("PATH", &path)
        .env("CONFIG_SHELL", SH)
        .env("SHELL", SH)
        .env("CC", "{root}/wb/cc")
        .env("CC_FOR_BUILD", "{root}/wb/cc")
        .env("C_INCLUDE_PATH", &cip),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make}/bin/make",
                "-j{jobs}",
                "SHELL={in:bash}/bin/bash",
                "CONFIG_SHELL={in:bash}/bin/bash",
                "MAKEINFO=true",
            ],
        )
        .env("PATH", &path)
        .env("C_INCLUDE_PATH", &cip),
    );
    steps.push(
        Step::run(
            "{src}",
            &[
                "{in:make}/bin/make",
                "SHELL={in:bash}/bin/bash",
                "MAKEINFO=true",
                "install",
                "prefix={out}",
            ],
        )
        .env("PATH", &path),
    );
    steps.push(Step::Require {
        paths: vec!["{out}/bin/make".into()],
        exec: true,
    });
    // [native-arch] the produced make is itself an ELF64 x86_64 binary — a NATIVE build
    // artifact, not an i686 cross one — asserted by the interned native readelf. Catch a
    // wrong-arch make HERE, at the rung that produced it, with a clear message, rather than
    // indirectly (a generic failure) in the heavy daily behavioral probe. Parity with
    // binutils-x86-64-native / gcc-x86-64-native, and directly on-point for a rung whose
    // whole purpose is proving a native /td/store toolchain build. The static native readelf
    // runs on the x86_64 sandbox host; grep is on base_path().
    steps.push(
        Step::run(
            "{out}",
            &[
                SH,
                "-c",
                "h=$('{in:binutils-x86-64-native}/bin/readelf' -h '{out}/bin/make'); \
                 printf '%s\\n' \"$h\" | grep -i 'class:'   | grep -qi 'ELF64'  || { echo 'make is not ELF64' >&2; exit 1; }; \
                 printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi 'x86-64' || { echo 'make is not x86-64' >&2; exit 1; }",
            ],
        )
        .env("PATH", &base_path()),
    );
    Recipe::mesboot("make-x86-64", "4.4.1")
        .native_inputs(&["gcc-x86-64-native", "binutils-x86-64-native", "glibc-x86-64"])
        .inputs_owned(base_inputs(&[
            "make-x86-64-source",
            "linux-headers-x86-64",
            "make",
        ]))
        .steps(steps)
        .checks(vec![RecipeCheck::daily(
            r#"
echo ">> recipe-check make-x86-64: build-plan --auto builds GNU make 4.4.1 on the /td/store native x86_64 toolchain (gcc-x86-64-native + binutils-x86-64-native + glibc-x86-64); make RUNS from /td/store in a /gnu/store-absent own-root and drives a build"
sh tests/make-x86-64-recipe-check.sh
"#,
        )])
}
