use crate::types::{Recipe, RecipeCheck};

pub fn recipe() -> Recipe {
    Recipe::rust("ripgrep", "14.1.1")
        .bins(&["rg"])
        .checks(vec![RecipeCheck::daily(r#"
recipe_crate_free_build ripgrep ripgrep-14.1.1 tests/ripgrep.lock ripgrep-source ripgrep
test -x "$ns/bin/rg" || { echo "FAIL: no rg binary at $ns/bin/rg" >&2; exit 1; }
tree="$PWD/.td-build-cache/ripgrep-crate-free/tree"; rm -rf "$tree"; mkdir -p "$tree/sub"; printf 'alpha line\nthe needle is here\nbeta line\n' > "$tree/sub/hay.txt"; printf 'nothing to see\n' > "$tree/other.txt"
found=`"$ns/bin/rg" needle "$tree"`
echo "$found" | grep -q 'needle' || { echo "FAIL: td-built rg did not find the needle line (got: $found)" >&2; exit 1; }
echo "$found" | grep -q 'other.txt' && { echo "FAIL: td-built rg matched the unrelated file" >&2; exit 1; }
rm -rf "$tree"
echo "  [DURABLE behavioral] the td-built rg found the needle line and not the unrelated file"
"#)])
}
