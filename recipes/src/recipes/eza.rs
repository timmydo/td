use crate::types::{Recipe, RecipeCheck};

pub fn recipe() -> Recipe {
    Recipe::rust("eza", "0.21.6")
        .bins(&["eza"])
        .no_default_features()
        .checks(vec![RecipeCheck::daily(r#"
recipe_crate_free_build eza eza-0.21.6 tests/eza.lock eza-source eza
test -x "$ns/bin/eza" || { echo "FAIL: no eza binary at $ns/bin/eza" >&2; exit 1; }
tree="$PWD/.td-build-cache/eza-crate-free/tree"; rm -rf "$tree"; mkdir -p "$tree"; : > "$tree/alpha.txt"; : > "$tree/beta.log"
listing=`"$ns/bin/eza" "$tree"`
echo "$listing" | grep -q 'alpha.txt' && echo "$listing" | grep -q 'beta.log' || { echo "FAIL: td-built eza did not list the directory entries (got: $listing)" >&2; exit 1; }
echo "$listing" | grep -q 'nonexistent' && { echo "FAIL: eza listed a file that does not exist" >&2; exit 1; }
rm -rf "$tree"
echo "  [DURABLE behavioral] the td-built eza listed the directory entries"
"#)])
}
