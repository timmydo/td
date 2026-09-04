use crate::ladder::{post_bootstrap_path, POST_BOOTSTRAP_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// Keep in lockstep with toolchain_x86_64::GLIBC_X86_64_STAGE and
// rust_toolchain::GLIBC_STAGE.
const GLIBC_STAGE: &str = "stage/td/store/glibc-2.41-x86_64";

/// The provenance scans every contract shares. The scanner's positive control
/// comes first: a NUL-filled probe longer than the tested binary with the
/// needle at its far end, so a scanner that stops at a NUL, or at the binary's
/// length, is caught before its two negative answers are trusted. The
/// negatives are no `/gnu/store` bytes and no Rust stage0 bytes.
fn provenance_scans(label: &str, binary: &str) -> String {
    let scanner = "{in:td-txt}/bin/td-txt";
    format!(
        "binary_size=$(wc -c < '{binary}'); \
         head -c \"$binary_size\" /dev/zero > '{{root}}/binary-scan-probe' || {{ echo 'could not create binary provenance control' >&2; exit 1; }}; \
         printf 'td-binary-scan-positive' >> '{{root}}/binary-scan-probe'; \
         set -- $(od -An -tx1 -N1 '{{root}}/binary-scan-probe'); \
         [ \"$1\" = 00 ] || {{ echo 'binary provenance control contains no NUL' >&2; exit 1; }}; \
         probe_size=$(wc -c < '{{root}}/binary-scan-probe'); \
         [ \"$probe_size\" -gt \"$binary_size\" ] || {{ echo 'binary provenance control does not reach beyond the tested binary size' >&2; exit 1; }}; \
         '{scanner}' grep -a -Fq -- td-binary-scan-positive '{{root}}/binary-scan-probe' || {{ echo 'binary provenance scanner missed its positive control' >&2; exit 1; }}; \
         '{scanner}' grep -a -Fq -- /gnu/store '{binary}'; scan=$?; \
         case \"$scan\" in 1) :;; 0) echo '{label} embeds /gnu/store bytes' >&2; exit 1;; *) echo 'binary provenance scan failed for {label}' >&2; exit 1;; esac; \
         stage0='{{in:rust-stage0}}'; stage0_base=${{stage0##*/}}; \
         '{scanner}' grep -a -Fq -- \"$stage0_base\" '{binary}'; scan=$?; \
         case \"$scan\" in 1) :;; 0) echo '{label} embeds Rust stage0 bytes' >&2; exit 1;; *) echo 'Rust stage0 provenance scan failed for {label}' >&2; exit 1;; esac; "
    )
}

fn dynamic_contract(label: &str, binary: &str, expected_needed: &str) -> Step {
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let scans = provenance_scans(label, binary);
    let glibc = format!("{{in:glibc-x86-64}}/{GLIBC_STAGE}");
    Step::run(
        "{root}",
        &[
            POST_BOOTSTRAP_SH,
            "-c",
            &format!(
                "[ -x '{binary}' ] || {{ echo '{label} output is not executable' >&2; exit 1; }}; \
                 {scans}\
                 h=$('{readelf}' -h '{binary}') || {{ echo 'readelf -h failed on {label}' >&2; exit 1; }}; \
                 printf '%s\\n' \"$h\" | grep -i 'class:' | grep -qi ELF64 || {{ echo '{label} is not ELF64' >&2; exit 1; }}; \
                 printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi x86-64 || {{ echo '{label} is not x86-64' >&2; exit 1; }}; \
                 p=$('{readelf}' -l '{binary}') || {{ echo 'readelf -l failed on {label}' >&2; exit 1; }}; \
                 printf '%s\\n' \"$p\" | grep -Fq '{glibc}/lib/ld-linux-x86-64.so.2' || {{ echo '{label} does not use the declared td glibc interpreter' >&2; exit 1; }}; \
                 d=$('{readelf}' -d '{binary}') || {{ echo 'readelf -d failed on {label}' >&2; exit 1; }}; \
                 needed=$(printf '%s\\n' \"$d\" | sed -n 's/^.*(NEEDED).*Shared library: \\[\\([^]]*\\)\\].*$/\\1/p' | sort); \
                 [ \"$needed\" = '{expected_needed}' ] || {{ echo \"{label} has an unexpected DT_NEEDED closure: $needed\" >&2; exit 1; }}; \
                 runpath=$(printf '%s\\n' \"$d\" | sed -n 's/^.*(RUNPATH).*Library runpath: \\[\\([^]]*\\)\\].*$/\\1/p'); \
                 [ -z \"$runpath\" ] || {{ echo \"{label} has an unexpected DT_RUNPATH: $runpath\" >&2; exit 1; }}; \
                 rpath=$(printf '%s\\n' \"$d\" | sed -n 's/^.*(RPATH).*Library rpath: \\[\\([^]]*\\)\\].*$/\\1/p'); \
                 [ \"$rpath\" = '{glibc}/lib' ] || {{ echo \"{label} has an unexpected DT_RPATH: $rpath\" >&2; exit 1; }}"
            ),
        ],
    )
    .env("PATH", &post_bootstrap_path())
}

/// The contract of a `static_link` Cargo output: the same provenance scans and
/// ELF64/x86-64 shape as the dynamic rungs, then no program interpreter, no
/// `DT_NEEDED`, no run-path, and a position-independent executable (`ET_DYN`
/// flagged `PIE`) rather than a fixed-address `ET_EXEC` — the static shape an
/// application package's validator admits, with the image base the kernel
/// still randomizes.
fn static_contract(label: &str, binary: &str) -> Step {
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let scans = provenance_scans(label, binary);
    Step::run(
        "{root}",
        &[
            POST_BOOTSTRAP_SH,
            "-c",
            &format!(
                "[ -x '{binary}' ] || {{ echo '{label} output is not executable' >&2; exit 1; }}; \
                 {scans}\
                 h=$('{readelf}' -h '{binary}') || {{ echo 'readelf -h failed on {label}' >&2; exit 1; }}; \
                 printf '%s\\n' \"$h\" | grep -i 'class:' | grep -qi ELF64 || {{ echo '{label} is not ELF64' >&2; exit 1; }}; \
                 printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi x86-64 || {{ echo '{label} is not x86-64' >&2; exit 1; }}; \
                 printf '%s\\n' \"$h\" | grep -i 'type:' | grep -q 'DYN' || {{ echo '{label} is not a position-independent (ET_DYN) static executable' >&2; exit 1; }}; \
                 p=$('{readelf}' -l '{binary}') || {{ echo 'readelf -l failed on {label}' >&2; exit 1; }}; \
                 printf '%s\\n' \"$p\" | grep -q 'INTERP' && {{ echo '{label} has a program interpreter' >&2; exit 1; }}; \
                 d=$('{readelf}' -d '{binary}' 2>&1) || {{ echo 'readelf -d failed on {label}' >&2; exit 1; }}; \
                 printf '%s\\n' \"$d\" | grep '(FLAGS_1)' | grep -q 'PIE' || {{ echo '{label} is ET_DYN without the PIE flag: a shared object, not a static PIE' >&2; exit 1; }}; \
                 printf '%s\\n' \"$d\" | grep -q '(NEEDED)' && {{ echo '{label} has a DT_NEEDED entry' >&2; exit 1; }}; \
                 printf '%s\\n' \"$d\" | grep -q '(RUNPATH)\\|(RPATH)' && {{ echo '{label} has a run-path' >&2; exit 1; }}; \
                 exit 0"
            ),
        ],
    )
    .env("PATH", &post_bootstrap_path())
}

pub fn recipe() -> Recipe {
    let rg = "{in:ripgrep}/bin/rg";
    let fd = "{in:fd}/bin/fd";
    let tn = "{in:tn}/bin/tn";
    let tmc = "{in:tmc}/bin/tmc";
    let fixture = "{root}/fixtures/known-needle.txt";
    let mut steps = vec![
        dynamic_contract("ripgrep", rg, "ld-linux-x86-64.so.2\nlibc.so.6"),
        dynamic_contract("fd", fd, "libc.so.6"),
        // The two terminal applications are GitHub commit archives rather than
        // crates.io packages, link `ring`'s C and assembly through the Cargo
        // build-script path, and are the first `static_link` Cargo outputs:
        // their contract is the static validator's shape, not the glibc one.
        static_contract("tn", tn),
        static_contract("tmc", tmc),
        Step::MkDir {
            path: "{root}/fixtures".into(),
        },
        Step::WriteFile {
            path: fixture.into(),
            content: "noise\nneedle\n".into(),
            exec: false,
        },
    ];
    steps.push(
        Step::run(
            "{root}",
            &[
                POST_BOOTSTRAP_SH,
                "-c",
                &format!(
                    "actual=$('{rg}' --color never --no-filename '^needle$' '{fixture}') || {{ echo 'ripgrep search failed' >&2; exit 1; }}; \
                     [ \"$actual\" = 'needle' ] || {{ echo \"ripgrep returned unexpected output: $actual\" >&2; exit 1; }}; \
                     actual=$('{fd}' --color never --absolute-path '^known-needle[.]txt$' '{{root}}/fixtures') || {{ echo 'fd search failed' >&2; exit 1; }}; \
                     [ \"$actual\" = '{fixture}' ] || {{ echo \"fd returned unexpected output: $actual\" >&2; exit 1; }}; \
                     usage=$('{tn}' --help 2>&1) || {{ echo 'tn --help failed' >&2; exit 1; }}; \
                     case \"$usage\" in *'Usage: tn'*) :;; *) echo \"tn --help returned unexpected output: $usage\" >&2; exit 1;; esac; \
                     usage=$('{tmc}' --help 2>&1) || {{ echo 'tmc --help failed' >&2; exit 1; }}; \
                     case \"$usage\" in *'Usage: tmc'*) :;; *) echo \"tmc --help returned unexpected output: $usage\" >&2; exit 1;; esac"
                ),
            ],
        )
        .env("PATH", &post_bootstrap_path()),
    );
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: ripgrep and fd are target-built auto graph nodes with the declared td glibc runtime closure, and tn and tmc are fully static Cargo outputs\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("rust-userland-auto-test", "1.0")
        .native_inputs(&[
            "ripgrep",
            "fd",
            "tn",
            "tmc",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
            "td-txt",
            // This boundary check needs the stage0 basename for its negative
            // byte scan; it never executes the bootstrap compiler.
            "rust-stage0",
        ])
        .steps(steps)
        .checks(vec![
            RecipeCheck::new(
                r#"
echo ">> recipe-check rust-userland-auto-test: build-plan --auto builds ripgrep, fd, tn and tmc with the source-built Rust/native toolchain, verifies the exact dynamic runtime closure of the first two and the static position-independent shape of the last two, and runs real searches and usage output with /gnu/store absent"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run rust-userland-auto-test 1
"#,
            )
            .with_runner(CheckRunner::BuildOnly),
        ])
}
