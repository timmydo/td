use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("td-ts-eval", "0.1.0").bins(&["td-ts-eval"])
}
