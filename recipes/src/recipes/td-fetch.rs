use crate::types::{Recipe, RecipeCheck};

pub fn recipe() -> Recipe {
    Recipe::rust("td-fetch", "0.1.0")
        .bins(&["td-fetch"])
        .checks(vec![RecipeCheck::pr(r#"
echo ">> recipe-check td-fetch: build td-fetch with a guix-free interned vendor tree"
recipe_vendor_tree_rust_build td-fetch "$PWD/fetch" "$PWD/.td-build-cache/crate-vendor/td-fetch" "$PWD/tests/td-fetch.lock" td-fetch-source td-fetch "$PWD/fetch/Cargo.lock" 70 td-fetch
rc=0; "$ns/bin/td-fetch" >/dev/null 2>&1 || rc=$?
test "$rc" = 2 || { echo "FAIL: the td-built td-fetch usage exit != 2 (got $rc)" >&2; exit 1; }
echo "  [DURABLE behavioral] the td-built td-fetch runs (usage exit 2)"
recipe_check_drv_repro
echo "PASS: td-fetch recipe check — td-fetch builds from a guix-free vendor tree, runs, and is reproducible."
"#)])
}
