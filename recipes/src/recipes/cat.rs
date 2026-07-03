use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("cat", "0.9.0").bins(&["cat"])
}
