use crate::types::{Recipe, RecipeCheck};

pub fn recipe() -> Recipe {
    Recipe::rust("td-vendor-demo", "0.1.0")
        .bins(&["td-vendor-demo"])
        .checks(vec![RecipeCheck::pr(r#"
echo ">> recipe-check td-vendor-demo: build a Rust crate with lock-pinned vendored deps"
recipe_local_crate_lock_build td-vendor-demo "$PWD/tests/vendor-demo" "$PWD/tests/td-vendor-demo.lock" td-vendor-demo-source td-vendor-demo td-vendor-demo
got=`"$ns/bin/td-vendor-demo"`
test "$got" = "2026 3.14159" || { echo "FAIL: td-vendor-demo printed '$got', expected '2026 3.14159'" >&2; exit 1; }
echo "  [DURABLE behavioral] the vendored binary runs and prints '$got' (itoa + ryu both exercised)"
recipe_check_drv_repro
echo "PASS: td-vendor-demo recipe check — build-recipe used vendored deps, the binary runs, and the output is reproducible."
"#)])
}
