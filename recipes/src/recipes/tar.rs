use crate::types::{Recipe, Source};

pub fn recipe() -> Recipe {
    Recipe::gnu("tar", "1.35").source(Source::one(
        "mirror://gnu/tar/tar-1.35.tar.xz",
        "05nw7q7sazkana11hnf3f77lmybw1j9j6lsk93bsxirf6hvzyqjd",
    ))
}
