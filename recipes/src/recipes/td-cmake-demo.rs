use crate::types::{Recipe, RecipeCheck};

pub fn recipe() -> Recipe {
    Recipe::cmake("td-cmake-demo", "0.1.0").checks(vec![RecipeCheck::pr(r#"
echo ">> recipe-check td-cmake-demo: build a cmake C project through td's cmake phase runner"
recipe_cmake_local_build td-cmake-demo tests/cmake-demo "$PWD/tests/td-cmake-demo.lock" td-cmake-demo-source td-cmake-demo td-cmake-hello "td cmake-build hello"
echo "PASS: td-cmake-demo recipe check — buildSystem cmake selects td's cmake phase runner, the binary runs, the build is reproducible, and the guix cmake-build-system oracle is distinct."
"#)])
}
