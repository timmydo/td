use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("td-subst", "0.1.0").bins(&["td-subst"])
}
