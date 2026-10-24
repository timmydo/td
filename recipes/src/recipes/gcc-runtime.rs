use crate::types::{Recipe, Step};

// Debian libgcc and startup objects, imported without compiling GCC.
pub fn recipe() -> Recipe {
    Recipe::mesboot("gcc-runtime", "14.2.0")
        .build_root()
        .steps(vec![Step::DebianRuntime {
            kind: "gcc-v1".into(),
        }])
}
