//! Greedy word wrap for the cell grid: text broken at whitespace into
//! rows of at most a given number of characters.

/// `text` broken at whitespace into rows of at most `columns` characters,
/// a run of whitespace read as one space and a word longer than a row
/// split across rows. Empty text is one empty row; no columns is the
/// whole text as one row, where splitting could never fit.
pub fn wrap(text: &str, columns: usize) -> Vec<String> {
    if columns == 0 {
        return vec![text.to_owned()];
    }
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut width = 0;
    for word in text.split_whitespace() {
        let length = word.chars().count();
        if width > 0 && width + 1 + length <= columns {
            row.push(' ');
            row.push_str(word);
            width += 1 + length;
            continue;
        }
        if width > 0 {
            rows.push(std::mem::take(&mut row));
        }
        let mut rest = word;
        while let Some((at, _)) = rest.char_indices().nth(columns) {
            let Some((head, tail)) = rest.split_at_checked(at) else {
                break;
            };
            rows.push(head.to_owned());
            rest = tail;
        }
        row.push_str(rest);
        width = rest.chars().count();
    }
    if width > 0 || rows.is_empty() {
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_fill_rows_and_whitespace_runs_are_one_space() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("one two three", 13), ["one two three"]);
        assert_eq!(wrap("  one \t two\n", 20), ["one two"]);
        assert_eq!(wrap("a b", 1), ["a", "b"]);
    }

    #[test]
    fn a_long_word_splits_and_its_tail_takes_the_next_word() {
        assert_eq!(wrap("abcdefgh ij", 3), ["abc", "def", "gh", "ij"]);
        assert_eq!(wrap("x abcdef y", 3), ["x", "abc", "def", "y"]);
        assert_eq!(wrap("abcd e", 3), ["abc", "d e"]);
    }

    #[test]
    fn width_counts_characters_not_bytes() {
        assert_eq!(wrap("ééé ü", 5), ["ééé ü"]);
        assert_eq!(wrap("éééé", 2), ["éé", "éé"]);
    }

    #[test]
    fn empty_text_is_one_empty_row_and_no_columns_is_one_row() {
        assert_eq!(wrap("", 10), [""]);
        assert_eq!(wrap("   ", 10), [""]);
        assert_eq!(wrap("one two", 0), ["one two"]);
    }

    /// td-pass's prompt case, which this wrap was lifted from.
    #[test]
    fn a_prompt_keeps_every_word_whole_within_its_columns() {
        let text = "create a portable vault: enroll the separate backup key";
        let rows = wrap(text, 20);
        assert_eq!(
            rows,
            [
                "create a portable",
                "vault: enroll the",
                "separate backup key"
            ]
        );
        assert_eq!(rows.join(" "), text);
        assert_eq!(wrap("0123456789ab cd", 5), ["01234", "56789", "ab cd"]);
        assert_eq!(wrap("exactly five", 5), ["exact", "ly", "five"]);
        assert_eq!(wrap("é é é", 3), ["é é", "é"]);
    }

    #[test]
    fn no_row_is_wider_than_the_columns() {
        let text = "the quick brown fox jumps over a lazy dog supercalifragilistic";
        for columns in 1..=20 {
            assert!(wrap(text, columns)
                .iter()
                .all(|row| row.chars().count() <= columns));
        }
    }
}
