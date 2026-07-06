use crate::types::{Recipe, RecipeCheck};

pub fn recipe() -> Recipe {
    Recipe::rust("youki", "0.6.0")
        .bins(&["youki"])
        .checks(vec![RecipeCheck::daily(r#"
recipe_crate_free_build youki youki-0.6.0 tests/youki.lock youki-source youki
test -x "$ns/bin/youki" || { echo "FAIL: no youki binary at $ns/bin/youki" >&2; exit 1; }
vout=$("$ns/bin/youki" --version 2>&1) || { echo "FAIL: youki --version exited nonzero" >&2; printf '%s\n' "$vout" >&2; exit 1; }
printf '%s\n' "$vout" | grep -qi 'youki' || { echo "FAIL: youki --version did not report youki" >&2; printf '%s\n' "$vout" >&2; exit 1; }
hout=$("$ns/bin/youki" --help 2>&1) || { echo "FAIL: youki --help exited nonzero" >&2; exit 1; }
printf '%s\n' "$hout" | grep -qiE '\bcreate\b' || { echo "FAIL: youki --help did not list the OCI create subcommand" >&2; printf '%s\n' "$hout" >&2; exit 1; }
echo "  [DURABLE behavioral] the td-built youki runs as an OCI runtime CLI"
"#)])
}
