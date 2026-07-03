use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("ripgrep", "14.1.1").bins(&["rg"])
}
