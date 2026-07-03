use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("sd", "1.0.0").bins(&["sd"])
}
