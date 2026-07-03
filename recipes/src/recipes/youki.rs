use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("youki", "0.6.0").bins(&["youki"])
}
