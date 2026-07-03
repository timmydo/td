use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("td-builder", "0.1.0").bins(&["td-builder"])
}
