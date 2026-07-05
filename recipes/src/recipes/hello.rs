use crate::types::{Recipe, RecipeCheck, Source};

// The `hello-perturbed` twin derives from this recipe (`super::hello::recipe()`),
// so base and twin stay field-identical except the twin's deliberate delta.
pub fn recipe() -> Recipe {
    Recipe::gnu("hello", "2.12.2").source(Source::one(
        "mirror://gnu/hello/hello-2.12.2.tar.gz",
        "1aqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js",
    ))
    .checks(vec![
        RecipeCheck::pr(r#"
echo ">> recipe-check hello: build-recipe with guix/Guile off PATH; hello runs; reproducible; self-discriminated by hello-perturbed"
recipe_cached_build hello "$PWD/tests/hello-no-guix.lock"
test "`LD_LIBRARY_PATH="$L" "$ns/bin/hello"`" = "Hello, world!" \
  || { echo "FAIL: hello did not greet" >&2; exit 1; }
echo "  [DURABLE behavioral] hello runs/ships from td's own store output"
cached_check "$spec" || exit 1
recipe_self_discriminates hello hello-perturbed
cached_clean
echo "PASS: hello recipe check — build-recipe assembles/realizes hello with guix/Guile off PATH; the binary runs, is reproducible, and hello-perturbed assembles a distinct .drv."
"#),
        RecipeCheck::daily(r#"
echo ">> recipe-check hello/store-native: /td/store source-bootstrap toolchain builds GNU hello"
sh tests/bootstrap-hello-corpus-store-native.sh
"#),
    ])
}
