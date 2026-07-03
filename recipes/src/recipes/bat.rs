use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("bat", "0.25.0")
        .bins(&["bat"])
        .no_default_features()
        .features(&["clap", "etcetera", "paging", "wild", "regex-fancy"])
}
