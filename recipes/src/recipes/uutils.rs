use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("uutils", "0.9.0").bins(&["coreutils"])
}
