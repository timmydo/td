use crate::types::{Recipe, Step};

// Debian glibc, headers and timezone tools, imported without compilation.
pub fn recipe() -> Recipe {
    Recipe::mesboot("glibc-x86-64", "2.41")
        .build_root()
        .steps(vec![Step::DebianRuntime {
            kind: "glibc-v2".into(),
        }])
}
