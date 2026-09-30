//! Which characters may appear in a line-oriented record. Shared source: the
//! compositor's control reports and td-term's screen reports (through td-ui)
//! filter with this one function, so the two cannot disagree.

/// Whether a character may appear in a record.
///
/// `char::is_control` is Unicode category Cc and is NOT the whole answer, which
/// an earlier draft of this assumed. Two more kinds matter and neither is Cc:
///
/// - U+2028 and U+2029 END A LINE for readers that are not `str::lines` —
///   Python's `splitlines` among them, and a program in Python is exactly the
///   reader this report is for. A title carrying one forges a record there
///   while looking harmless here.
/// - the bidirectional controls and the zero-width characters change how the
///   line READS without changing what it contains. That is the same objection
///   the carriage return already answers: a person reading `td-ctl layout` out
///   of a terminal should see what the record says.
///
/// A list rather than a category test, because neither the compositor nor
/// td-ui has Unicode tables and neither will grow one for a label field, nor
/// for the screen rows and program names td-term reports through it. Ordinary
/// text in any script survives; what does not is the set below, named with its
/// reason.
pub fn reportable(character: char) -> bool {
    !character.is_control()
        && !matches!(character,
            // Line and paragraph separators (Zl, Zp).
            '\u{2028}' | '\u{2029}'
            // Zero-width and the directional marks (U+200B-200F).
            | '\u{200b}'..='\u{200f}'
            // Bidirectional embedding, override and pop (U+202A-202E).
            | '\u{202a}'..='\u{202e}'
            // Bidirectional isolates (U+2066-2069).
            | '\u{2066}'..='\u{2069}'
            // The remaining invisibles a title has no use for.
            | '\u{00ad}' | '\u{061c}' | '\u{180e}' | '\u{feff}')
}

#[cfg(test)]
mod tests {
    use super::reportable;

    #[test]
    fn the_named_invisibles_and_separators_are_refused_and_text_survives() {
        for character in [
            '\n', '\r', '\u{7f}', '\u{2028}', '\u{2029}', '\u{200b}', '\u{200f}', '\u{202a}',
            '\u{202e}', '\u{2066}', '\u{2069}', '\u{00ad}', '\u{061c}', '\u{180e}', '\u{feff}',
        ] {
            assert!(!reportable(character), "{character:?}");
        }
        for character in ['a', ' ', 'é', '日', 'Ω', '\u{2010}', '\u{2030}'] {
            assert!(reportable(character), "{character:?}");
        }
    }
}
