use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("td-fetch", "0.1.0").bins(&["td-fetch"])
}
