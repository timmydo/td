use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("td-russh-demo", "0.1.0").bins(&["td-russh-demo"])
}
