use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("procs", "0.14.10").bins(&["procs"])
}
