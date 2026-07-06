use crate::types::{Recipe, RecipeCheck};

pub fn recipe() -> Recipe {
    Recipe::rust("procs", "0.14.10")
        .bins(&["procs"])
        .checks(vec![RecipeCheck::daily(r#"
recipe_crate_free_build procs procs-0.14.10 tests/procs.lock procs-source procs
test -x "$ns/bin/procs" || { echo "FAIL: no procs binary at $ns/bin/procs" >&2; exit 1; }
"$ns/bin/procs" --version >/dev/null 2>&1 || { echo "FAIL: td-built procs --version failed" >&2; exit 1; }
ptab=`"$ns/bin/procs" </dev/null 2>/dev/null || true`
printf '%s\n' "$ptab" | grep -qiE 'PID|Command' || { echo "FAIL: td-built procs produced no process-table header reading /proc" >&2; exit 1; }
nrows=`printf '%s\n' "$ptab" | grep -cE '^[[:space:]]*[0-9]+' || true`
echo "  [DURABLE behavioral] the td-built procs ran and read /proc into a process table ($nrows rows)"
"#)])
}
