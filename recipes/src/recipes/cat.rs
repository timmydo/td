use crate::types::{Recipe, RecipeCheck};

pub fn recipe() -> Recipe {
    Recipe::rust("cat", "0.9.0")
        .bins(&["cat"])
        .checks(vec![RecipeCheck::daily(r#"
recipe_crate_free_build cat uu_cat-0.9.0 tests/cat-uutils.lock cat-source cat
bin="$ns/bin/cat"
test -x "$bin" || { echo "FAIL: no uu_cat 'cat' binary at $bin" >&2; exit 1; }
w="$PWD/.td-build-cache/cat-crate-free/work"; rm -rf "$w"; mkdir -p "$w"
printf 'hello from td-built cat\nline two\n' > "$w/in.txt"
got=`"$bin" "$w/in.txt"`
test "$got" = "$(printf 'hello from td-built cat\nline two')" || { echo "FAIL: td-built cat did not round-trip the file (got: $got)" >&2; exit 1; }
piped=`printf 'piped-in\n' | "$bin"`
test "$piped" = "piped-in" || { echo "FAIL: td-built cat did not round-trip stdin (got: $piped)" >&2; exit 1; }
rm -rf "$w"
echo "  [DURABLE behavioral] the td-built uutils 'cat' round-trips a file AND a stdin pipe"
"#)])
}
