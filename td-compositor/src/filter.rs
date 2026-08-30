//! The bounded ASCII query shared by td's launcher and portal lists.

pub const MAX_QUERY_BYTES: usize = 64;

/// Add one character using the launcher's exact input policy.
pub fn insert(query: &mut String, character: char) -> bool {
    if !character.is_ascii() || character.is_ascii_control() || query.len() >= MAX_QUERY_BYTES {
        return false;
    }
    query.push(character.to_ascii_lowercase());
    true
}

/// Every ASCII-whitespace-separated term must occur in the searchable text.
pub fn matches(search: &str, query: &str) -> bool {
    query
        .split_ascii_whitespace()
        .all(|term| search.contains(term))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_input_and_all_term_matching_are_exact() {
        let mut query = String::new();
        for character in "Fi Re".chars() {
            assert!(insert(&mut query, character));
        }
        assert_eq!(query, "fi re");
        assert!(matches("open firefox report", &query));
        assert!(!matches("open report", &query));
        assert!(!insert(&mut query, '\n'));

        query.clear();
        for _ in 0..MAX_QUERY_BYTES {
            assert!(insert(&mut query, 'x'));
        }
        assert!(!insert(&mut query, 'y'));
        assert_eq!(query.len(), MAX_QUERY_BYTES);
    }
}
