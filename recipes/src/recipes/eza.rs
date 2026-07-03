use crate::types::Recipe;

pub fn recipe() -> Recipe {
    Recipe::rust("eza", "0.21.6").bins(&["eza"]).no_default_features()
}
