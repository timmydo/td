use crate::shared::{BUILD_ROOT_GLIBC, BUILD_ROOT_PATH as PATH, BUILD_ROOT_SH as SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// rust-userland-auto-test: the runtime contract of the Rust userland the
// auto graph builds. ripgrep and fd hold td glibc's exact dynamic closure,
// td-news and td-mail are td-owned static PIEs, and all four run.
//
// Every step runs in the Debian build root under its bash with PATH the
// root's /usr/bin alone, and grades with the root's readelf, grep and sed:
// no subject is on PATH, and each is reached by its store path.

/// The subjects, and td glibc for its loader's path.
const INPUTS: &[&str] = &["ripgrep", "fd", "td-news", "td-mail", "glibc-x86-64"];

/// The provenance scans every contract shares. The scanner's positive control
/// comes first: a NUL-filled probe longer than the tested binary with the
/// needle at its far end, so a scanner that stops at a NUL, or at the binary's
/// length, is caught before its negative answer is trusted: no `/gnu/store`
/// bytes.
fn provenance_scans(label: &str, binary: &str) -> String {
    let scanner = "/usr/bin/grep";
    format!(
        "binary_size=$(wc -c < '{binary}'); \
         head -c \"$binary_size\" /dev/zero > '{{root}}/binary-scan-probe' || {{ echo 'could not create binary provenance control' >&2; exit 1; }}; \
         printf 'td-binary-scan-positive' >> '{{root}}/binary-scan-probe'; \
         set -- $(od -An -tx1 -N1 '{{root}}/binary-scan-probe'); \
         [ \"$1\" = 00 ] || {{ echo 'binary provenance control contains no NUL' >&2; exit 1; }}; \
         probe_size=$(wc -c < '{{root}}/binary-scan-probe'); \
         [ \"$probe_size\" -gt \"$binary_size\" ] || {{ echo 'binary provenance control does not reach beyond the tested binary size' >&2; exit 1; }}; \
         '{scanner}' -a -Fq -- td-binary-scan-positive '{{root}}/binary-scan-probe' || {{ echo 'binary provenance scanner missed its positive control' >&2; exit 1; }}; \
         '{scanner}' -a -Fq -- /gnu/store '{binary}'; scan=$?; \
         case \"$scan\" in 1) :;; 0) echo '{label} embeds /gnu/store bytes' >&2; exit 1;; *) echo 'binary provenance scan failed for {label}' >&2; exit 1;; esac; "
    )
}

fn dynamic_contract(label: &str, binary: &str, expected_needed: &str) -> Step {
    let readelf = "/usr/bin/readelf";
    let scans = provenance_scans(label, binary);
    let glibc = BUILD_ROOT_GLIBC;
    Step::run(
        "{root}",
        &[
            SH,
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
    .env("PATH", PATH)
}

/// The contract of a td-owned static program the Cargo runner links
/// (`Recipe::rust` with `static_link`, the shape td-taskmgr wears): the
/// same provenance scans and ELF64/x86-64 shape as the dynamic rungs,
/// then a static PIE — `ET_DYN` with no program interpreter, no
/// `DT_NEEDED` and no run-path, which is what rustc's `+crt-static`
/// emits for a position-independent target; a static executable has no
/// dynamic section for either to live in, and `readelf -d` says so by
/// printing none. The application package's validator admits this shape
/// as it admits direct rustc's fixed-address one; the runner's install
/// check is the producer's check, this one the consumer's.
fn static_contract(label: &str, binary: &str) -> Step {
    let readelf = "/usr/bin/readelf";
    let scans = provenance_scans(label, binary);
    let (elf_type, shape_name) = ("DYN", "a static PIE (ET_DYN) executable");
    Step::run(
        "{root}",
        &[
            SH,
            "-c",
            &format!(
                "[ -x '{binary}' ] || {{ echo '{label} output is not executable' >&2; exit 1; }}; \
                 {scans}\
                 h=$('{readelf}' -h '{binary}') || {{ echo 'readelf -h failed on {label}' >&2; exit 1; }}; \
                 printf '%s\\n' \"$h\" | grep -i 'class:' | grep -qi ELF64 || {{ echo '{label} is not ELF64' >&2; exit 1; }}; \
                 printf '%s\\n' \"$h\" | grep -i 'machine:' | grep -qi x86-64 || {{ echo '{label} is not x86-64' >&2; exit 1; }}; \
                 printf '%s\\n' \"$h\" | grep -qE 'Type:[[:space:]]+{elf_type}([[:space:]]|$)' || {{ echo '{label} is not {shape_name}' >&2; exit 1; }}; \
                 p=$('{readelf}' -l '{binary}') || {{ echo 'readelf -l failed on {label}' >&2; exit 1; }}; \
                 printf '%s\\n' \"$p\" | grep -q 'INTERP' && {{ echo '{label} has a program interpreter' >&2; exit 1; }}; \
                 d=$('{readelf}' -d '{binary}' 2>&1) || {{ echo 'readelf -d failed on {label}' >&2; exit 1; }}; \
                 printf '%s\\n' \"$d\" | grep -q '(NEEDED)' && {{ echo '{label} has a DT_NEEDED entry' >&2; exit 1; }}; \
                 printf '%s\\n' \"$d\" | grep -q '(RUNPATH)\\|(RPATH)' && {{ echo '{label} has a run-path' >&2; exit 1; }}; \
                 exit 0"
            ),
        ],
    )
    .env("PATH", PATH)
}

pub fn recipe() -> Recipe {
    let rg = "{in:ripgrep}/bin/rg";
    let fd = "{in:fd}/bin/fd";
    let news = "{in:td-news}/bin/td-news";
    let mail = "{in:td-mail}/bin/td-mail";
    let fixture = "{root}/fixtures/known-needle.txt";
    let mut steps = vec![
        dynamic_contract("ripgrep", rg, "ld-linux-x86-64.so.2\nlibc.so.6"),
        dynamic_contract("fd", fd, "libc.so.6"),
        // The two applications are td's own root crates built static from
        // the checkout by the Cargo runner over td-ui (APPLICATIONS.md
        // §W.8): their contract is the td-owned static PIE, not the glibc
        // one.
        static_contract("td-news", news),
        static_contract("td-mail", mail),
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
                SH,
                "-c",
                &format!(
                    "actual=$('{rg}' --color never --no-filename '^needle$' '{fixture}') || {{ echo 'ripgrep search failed' >&2; exit 1; }}; \
                     [ \"$actual\" = 'needle' ] || {{ echo \"ripgrep returned unexpected output: $actual\" >&2; exit 1; }}; \
                     actual=$('{fd}' --color never --absolute-path '^known-needle[.]txt$' '{{root}}/fixtures') || {{ echo 'fd search failed' >&2; exit 1; }}; \
                     [ \"$actual\" = '{fixture}' ] || {{ echo \"fd returned unexpected output: $actual\" >&2; exit 1; }}; \
                     usage=$('{news}' --help 2>&1) || {{ echo 'td-news --help failed' >&2; exit 1; }}; \
                     case \"$usage\" in *'Usage: td-news'*) :;; *) echo \"td-news --help returned unexpected output: $usage\" >&2; exit 1;; esac; \
                     usage=$('{mail}' --help 2>&1) || {{ echo 'td-mail --help failed' >&2; exit 1; }}; \
                     case \"$usage\" in *'Usage: td-mail'*) :;; *) echo \"td-mail --help returned unexpected output: $usage\" >&2; exit 1;; esac"
                ),
            ],
        )
        .env("PATH", PATH),
    );
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: ripgrep and fd are target-built auto graph nodes with the declared td glibc runtime closure, td-news and td-mail are fully static PIEs over td-ui\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("rust-userland-auto-test", "1.0")
        .build_root()
        .native_inputs(INPUTS)
        .steps(steps)
        .checks(vec![
            RecipeCheck::new(
                r#"
echo ">> recipe-check rust-userland-auto-test: build-plan --auto builds ripgrep, fd, td-news and td-mail with upstream Rust through td-link, verifies the exact dynamic runtime closure of the first two, the td-owned static PIE shape of the last two, and runs real searches and usage output, graded in the Debian build root"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run rust-userland-auto-test 1
"#,
            )
            .with_runner(CheckRunner::BuildOnly),
        ])
}

#[cfg(test)]
mod tests {
    /// The four programs are graded only by the root's bash and tools.
    #[test]
    fn the_userland_is_graded_only_by_the_roots_bash_and_tools() {
        crate::shared::assert_build_root_test(
            &super::recipe(),
            super::INPUTS,
            &["ripgrep", "fd", "td-news", "td-mail"],
        );
    }
}
