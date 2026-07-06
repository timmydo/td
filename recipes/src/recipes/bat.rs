use crate::types::{Recipe, RecipeCheck};

pub fn recipe() -> Recipe {
    Recipe::rust("bat", "0.25.0")
        .bins(&["bat"])
        .no_default_features()
        .features(&["clap", "etcetera", "paging", "wild", "regex-fancy"])
        .checks(vec![RecipeCheck::daily(r#"
recipe_crate_free_build bat bat-0.25.0 tests/bat.lock bat-source bat
test -x "$ns/bin/bat" || { echo "FAIL: no bat binary at $ns/bin/bat" >&2; exit 1; }
btmp="$PWD/.td-build-cache/bat-crate-free/btmp"; rm -rf "$btmp"; mkdir -p "$btmp"
printf 'hello from td-built bat\nsecond line\n' > "$btmp/sample.txt"
got=`"$ns/bin/bat" --style=plain --paging=never --color=never "$btmp/sample.txt"`
echo "$got" | grep -q 'hello from td-built bat' && echo "$got" | grep -q 'second line' || { echo "FAIL: td-built bat did not print the file contents (got: $got)" >&2; exit 1; }
rm -rf "$btmp"
echo "  [DURABLE behavioral] the td-built bat printed the file contents"
"#)])
}
