use crate::ladder::{post_rust_inputs, post_rust_tool_farm, POST_RUST_SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// The build-only gawk's realized output, run on the same post-Rust tool farm
// its consumers use, with itself as the farm's awk: the constructs configure
// scripts and config.status lean on, system() and pipes through td-sh, its
// version, and a static x86-64 ELF naming neither a foreign store nor the
// bootstrap gawk and make that built it.
pub fn recipe() -> Recipe {
    let gawk = "{in:gawk-x86-64-self}/bin/gawk";
    let readelf = "{in:binutils-x86-64-self}/bin/readelf";
    let script = format!(
        "set -e; \
         out=$(printf '%s\\n' 'a:1' 'b:22' 'a:333' | awk -F: -v tag=n \
           '{{ s[$1] += length($2) }} END {{ for (k in s) printf \"%s=%d%s\\n\", k, s[k], tag }}' | sort); \
         [ \"$out\" = \"$(printf 'a=4n\\nb=2n')\" ] || {{ echo \"gawk arrays/printf: $out\" >&2; exit 1; }}; \
         out=$(echo 'x@@y@z' | awk '{{ n = gsub(/@+/, \"-\"); print n, $0 }}'); \
         [ \"$out\" = '2 x-y-z' ] || {{ echo \"gawk gsub: $out\" >&2; exit 1; }}; \
         out=$(echo 'abbbc' | awk '/^ab{{3}}c$/ {{ print \"interval\" }}'); \
         [ \"$out\" = interval ] || {{ echo \"gawk regex interval: $out\" >&2; exit 1; }}; \
         printf '%s\\n' one two > lines; \
         out=$(awk 'BEGIN {{ while ((getline l < \"lines\") > 0) n++; print n }}'); \
         [ \"$out\" = 2 ] || {{ echo \"gawk getline: $out\" >&2; exit 1; }}; \
         out=$(awk 'BEGIN {{ if (system(\"true\") != 0) exit 1; \
           \"echo piped\" | getline l; if (l != \"piped\") exit 2; \
           print \"to-cat\" | \"cat\"; if (close(\"cat\") != 0) exit 3 }}') || \
           {{ echo \"gawk system/pipes through the shell failed: $?\" >&2; exit 1; }}; \
         [ \"$out\" = to-cat ] || {{ echo \"gawk output pipe: $out\" >&2; exit 1; }}; \
         v=$('{gawk}' --version); \
         case $v in 'GNU Awk 5.4.1'*) ;; *) echo \"gawk version: $v\" >&2; exit 1;; esac; \
         h=$('{readelf}' -h -l '{gawk}'); \
         case $h in *'ELF64'*) ;; *) echo 'gawk is not ELF64' >&2; exit 1;; esac; \
         case $h in *'Advanced Micro Devices X86-64'*) ;; *) echo 'gawk is not x86-64' >&2; exit 1;; esac; \
         case $h in *INTERP*) echo 'gawk is not static' >&2; exit 1;; esac; \
         for ref in /gnu/store -gawk-mesboot- -make-x86-64-; do \
           r=0; grep -a -q -F -e \"$ref\" '{gawk}' || r=$?; \
           [ \"$r\" = 1 ] || {{ echo \"gawk names $ref (grep status $r)\" >&2; exit 1; }}; \
         done"
    );
    let steps = vec![
        Step::Require {
            paths: vec![gawk.into(), "{in:gawk-x86-64-self}/bin/awk".into()],
            exec: true,
        },
        post_rust_tool_farm("{in:gawk-x86-64-self}/bin/awk"),
        Step::MkDir {
            path: "{root}/test".into(),
        },
        Step::run("{root}/test", &[POST_RUST_SH, "-c", &script]).env("PATH", "{tools}"),
        Step::MkDir {
            path: "{out}".into(),
        },
        Step::WriteFile {
            path: "{out}/result".into(),
            content: "PASS: the build-only GNU Awk 5.4.1 is a static x86-64 ELF and ran \
                      configure's awk constructs on the post-Rust tool farm\n"
                .into(),
            exec: false,
        },
        Step::Require {
            paths: vec!["{out}/result".into()],
            exec: false,
        },
    ];

    Recipe::mesboot("gawk-x86-64-self-test", "1.0")
        .native_inputs(&post_rust_inputs(
            "gawk-x86-64-self",
            &["binutils-x86-64-self"],
        ))
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check gawk-x86-64-self-test: run the build-only GNU Awk on the post-Rust tool farm and check its ELF"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run gawk-x86-64-self-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}
