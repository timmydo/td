use crate::types::{Recipe, RecipeCheck};

pub fn recipe() -> Recipe {
    Recipe::rust("uutils", "0.9.0")
        .bins(&["coreutils"])
        .checks(vec![RecipeCheck::daily(r#"
recipe_crate_free_build uutils coreutils-0.9.0 tests/uutils-coreutils.lock uutils-source uutils
bin="$ns/bin/coreutils"
test -x "$bin" || { echo "FAIL: no coreutils multicall binary at $bin" >&2; exit 1; }
w="$PWD/.td-build-cache/uutils-crate-free/work"; rm -rf "$w"; mkdir -p "$w"
"$bin" mkdir "$w/sub" || { echo "FAIL: multicall mkdir" >&2; exit 1; }
printf 'hello from td-built coreutils\nline two\n' > "$w/f.txt"
"$bin" cp "$w/f.txt" "$w/sub/g.txt" || { echo "FAIL: multicall cp" >&2; exit 1; }
got=`"$bin" cat "$w/sub/g.txt"`
test "$got" = "$(printf 'hello from td-built coreutils\nline two')" || { echo "FAIL: coreutils cat did not round-trip the copied file (got: $got)" >&2; exit 1; }
"$bin" ls "$w/sub" | grep -qx 'g.txt' || { echo "FAIL: coreutils ls did not list the copied file" >&2; exit 1; }
"$bin" mv "$w/sub/g.txt" "$w/sub/h.txt" || { echo "FAIL: multicall mv" >&2; exit 1; }
test -e "$w/sub/h.txt" -a ! -e "$w/sub/g.txt" || { echo "FAIL: coreutils mv did not move the file" >&2; exit 1; }
"$bin" rm "$w/sub/h.txt" || { echo "FAIL: multicall rm" >&2; exit 1; }
test ! -e "$w/sub/h.txt" || { echo "FAIL: coreutils rm did not remove the file" >&2; exit 1; }
rm -rf "$w"
echo "  [DURABLE behavioral] the td-built coreutils multicall binary dispatches mkdir/cp/cat/ls/mv/rm"
"#)])
}
