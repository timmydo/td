use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("td-vendor-demo", "0.1.0").bins(&["td-vendor-demo"])
}
