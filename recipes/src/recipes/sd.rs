use crate::types::{Recipe, RecipeCheck};

pub fn recipe() -> Recipe {
    Recipe::rust("sd", "1.0.0")
        .bins(&["sd"])
        .checks(vec![RecipeCheck::daily(r#"
recipe_crate_free_build sd sd-1.0.0 tests/sd.lock sd-source sd
test -x "$ns/bin/sd" || { echo "FAIL: no sd binary at $ns/bin/sd" >&2; exit 1; }
got=`printf 'hello world\n' | "$ns/bin/sd" 'world' 'there'`
test "$got" = "hello there" || { echo "FAIL: td-built sd did not replace world->there (got: $got)" >&2; exit 1; }
unchanged=`printf 'hello world\n' | "$ns/bin/sd" 'zzznomatch' 'X'`
test "$unchanged" = "hello world" || { echo "FAIL: sd altered input on a non-matching pattern (got: $unchanged)" >&2; exit 1; }
echo "  [DURABLE behavioral] the td-built sd replaced world->there and left a non-match unchanged"
"#)])
}
