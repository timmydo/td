use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("td-feed", "0.1.0").bins(&["td-feed"])
}
