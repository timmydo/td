use crate::types::{Recipe, RecipeCheck};

pub fn recipe() -> Recipe {
    Recipe::rust("fd", "10.2.0")
        .bins(&["fd"])
        .no_default_features()
        .features(&["completions"])
        .checks(vec![RecipeCheck::daily(r#"
recipe_crate_free_build fd fd-find-10.2.0 tests/fd.lock fd-source fd
test -x "$ns/bin/fd" || { echo "FAIL: no fd binary at $ns/bin/fd" >&2; exit 1; }
tree="$PWD/.td-build-cache/fd-crate-free/tree"; rm -rf "$tree"; mkdir -p "$tree/sub"; : > "$tree/foo.txt"; : > "$tree/bar.log"; : > "$tree/sub/needle.txt"
found=`"$ns/bin/fd" needle "$tree"`
echo "$found" | grep -q 'needle.txt' || { echo "FAIL: td-built fd did not find sub/needle.txt (got: $found)" >&2; exit 1; }
echo "$found" | grep -q 'foo.txt' && { echo "FAIL: td-built fd matched an unrelated file" >&2; exit 1; }
rm -rf "$tree"
echo "  [DURABLE behavioral] the td-built fd recursively found sub/needle.txt and only it"
"#)])
}
