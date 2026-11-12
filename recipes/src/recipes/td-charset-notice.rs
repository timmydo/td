use crate::types::{Recipe, Step};

/// The notice of the WHATWG data td-charset compiles into its consumers (its
/// encoding indexes and labels, BSD-3-Clause, whose second clause asks a
/// binary distribution to carry it), as data a package copies beside what
/// it ships: td-charset's retained `data/LICENSE-WHATWG`, byte for byte
/// (td-charset/DESIGN.md). A recipe of its own because an application
/// package's validator admits only copies before it, not a written file.
/// The step expands only its own tokens (`{out}`, `{in:NAME}` and the
/// like) and passes any other brace through; the text holds no brace at
/// all, which the test keeps, so nothing in it can become one.
const NOTICE: &str = "{out}/share/licenses/td-charset/LICENSE-WHATWG";
const LICENSE: &str = include_str!("../../../td-charset/data/LICENSE-WHATWG");

pub fn recipe() -> Recipe {
    Recipe::mesboot("td-charset-notice", "1").steps(vec![Step::WriteFile {
        path: NOTICE.into(),
        content: LICENSE.into(),
        exec: false,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notice_is_td_charsets_retained_licence_byte_for_byte() {
        let retained: &[u8] = include_bytes!("../../../td-charset/data/LICENSE-WHATWG");
        assert!(retained.starts_with("Copyright © WHATWG".as_bytes()));
        assert!(
            !retained.contains(&b'{'),
            "a brace could spell a token the step expands"
        );
        let recipe = recipe();
        assert_eq!(recipe.inputs, None);
        assert_eq!(recipe.native_inputs, None);
        let steps = recipe.steps.expect("steps");
        assert!(matches!(
            steps.as_slice(),
            [Step::WriteFile { path, content, exec: false }]
                if path == "{out}/share/licenses/td-charset/LICENSE-WHATWG"
                    && content.as_bytes() == retained
        ));
    }
}
