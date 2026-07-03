use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("fd", "10.2.0")
        .bins(&["fd"])
        .no_default_features()
        .features(&["completions"])
}
