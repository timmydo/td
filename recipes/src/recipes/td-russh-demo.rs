use crate::types::{Recipe, RecipeCheck};

pub fn recipe() -> Recipe {
    Recipe::rust("td-russh-demo", "0.1.0")
        .bins(&["td-russh-demo"])
        .checks(vec![RecipeCheck::daily(r#"
echo ">> recipe-check td-russh-demo: build and run a russh client/server loopback round-trip"
recipe_vendor_tree_rust_build td-russh-demo "$PWD/tests/russh-demo" "$PWD/.td-build-cache/crate-vendor/russh" "$PWD/tests/td-russh-demo.lock" td-russh-demo-source td-russh-demo "$PWD/tests/russh-demo/Cargo.lock" 150 td-russh-demo
got=`"$ns/bin/td-russh-demo" 2>"$scratch/run.err"` || { echo "FAIL: the td-built russh binary failed:" >&2; tail -5 "$scratch/run.err" >&2; exit 1; }
echo "$got" | grep -q '^td-russh-ok: ping$' || { echo "FAIL: russh round-trip did not return the expected reply (got: $got)" >&2; cat "$scratch/run.err" >&2; exit 1; }
echo "  [DURABLE behavioral] the td-built russh binary ran a full SSH round-trip over loopback: '$got'"
recipe_check_drv_repro
echo "PASS: td-russh-demo recipe check — the SSH round-trip crate builds with guix-free crates, runs, and is reproducible."
"#)])
}
