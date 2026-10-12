#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
fn key(id: u8, name: &str, sort_order: u32) -> Key<'_> {
    Key::new(MailboxId::from_bytes([id; 16]), name, sort_order).unwrap()
}
fn compare(left: &str, right: &str, collation: Option<&str>) -> Ordering {
    let comparators = [Comparator::new("name", true, collation).unwrap()];
    Plan::new(&comparators)
        .unwrap()
        .compare(key(1, left, 0), key(1, right, 0))
        .unwrap()
}
#[test]
fn literal_collation_oracles_distinguish_case_punctuation_and_unicode() {
    for (left, right, unicode, octet, ascii) in [
        ("A", "a", Ordering::Equal, Ordering::Less, Ordering::Equal),
        ("[", "a", Ordering::Less, Ordering::Less, Ordering::Greater),
        ("Z", "_", Ordering::Greater, Ordering::Less, Ordering::Less),
        (
            "\u{c9}",
            "é",
            Ordering::Equal,
            Ordering::Less,
            Ordering::Less,
        ),
        (
            "\u{130}",
            "i",
            Ordering::Equal,
            Ordering::Greater,
            Ordering::Greater,
        ),
        (
            "\u{212a}",
            "k",
            Ordering::Equal,
            Ordering::Greater,
            Ordering::Greater,
        ),
        (
            "\u{df}",
            "ss",
            Ordering::Greater,
            Ordering::Greater,
            Ordering::Greater,
        ),
        (
            "é",
            "e\u{301}",
            Ordering::Greater,
            Ordering::Greater,
            Ordering::Greater,
        ),
        (
            "\u{3c2}",
            "\u{3c3}",
            Ordering::Less,
            Ordering::Less,
            Ordering::Less,
        ),
        ("", "a", Ordering::Less, Ordering::Less, Ordering::Less),
        ("a", "aa", Ordering::Less, Ordering::Less, Ordering::Less),
    ] {
        assert_eq!(
            compare(left, right, None),
            unicode,
            "default {left}/{right}"
        );
        assert_eq!(
            compare(left, right, Some("i;octet")),
            octet,
            "octet {left}/{right}"
        );
        assert_eq!(
            compare(left, right, Some("i;ascii-casemap")),
            ascii,
            "ascii {left}/{right}"
        );
    }
}
#[test]
fn defaults_and_explicit_sorts_keep_ascending_id_ties() {
    let mut keys = [
        key(5, "beta", 0),
        key(2, "Alpha", 0),
        key(1, "ALPHA", 0),
        key(4, "a", 2),
        key(3, "z", 1),
    ];
    let plan = Plan::new(&[]).unwrap();
    keys.sort_by(|a, b| plan.compare(*a, *b).unwrap());
    assert_eq!(
        keys.iter().map(|key| key.id).collect::<Vec<_>>(),
        [1, 2, 5, 3, 4].map(|id| MailboxId::from_bytes([id; 16]))
    );
    let comparators = [Comparator::new("name", false, None).unwrap()];
    let plan = Plan::new(&comparators).unwrap();
    keys.reverse();
    keys.sort_by(|a, b| plan.compare(*a, *b).unwrap());
    assert_eq!(
        keys.iter().map(|key| key.id).collect::<Vec<_>>(),
        [3, 5, 1, 2, 4].map(|id| MailboxId::from_bytes([id; 16]))
    );
    let comparators = [
        Comparator::new("sortOrder", false, Some("unknown-but-ignored")).unwrap(),
        Comparator::new("name", true, None).unwrap(),
    ];
    let plan = Plan::new(&comparators).unwrap();
    keys.reverse();
    keys.sort_by(|a, b| plan.compare(*a, *b).unwrap());
    assert_eq!(
        keys.iter().map(|key| key.id).collect::<Vec<_>>(),
        [4, 3, 1, 2, 5].map(|id| MailboxId::from_bytes([id; 16]))
    );
}
#[test]
fn comparator_and_key_bounds_are_exact() {
    assert!(matches!(
        Comparator::new("id", true, None),
        Err(Error::UnsupportedSort)
    ));
    for collation in ["", "i;unicode-casemap", "I;OCTET", "unknown"] {
        assert!(matches!(
            Comparator::new("name", true, Some(collation)),
            Err(Error::UnsupportedSort)
        ));
    }
    let mut comparators = [Comparator::new("sortOrder", true, None).unwrap(); MAX_COMPARATORS + 1];
    comparators[MAX_COMPARATORS - 1] = Comparator::new("name", true, None).unwrap();
    let plan = Plan::new(&comparators[..MAX_COMPARATORS]).unwrap();
    assert_eq!(
        plan.compare(key(1, "z", 0), key(2, "a", 0)).unwrap(),
        Ordering::Greater
    );
    assert!(matches!(
        Plan::new(&comparators),
        Err(Error::UnsupportedSort)
    ));
    let mut exact = "é".repeat(MAX_MAILBOX_NAME / 2);
    if !MAX_MAILBOX_NAME.is_multiple_of(2) {
        exact.push('a');
    }
    assert_eq!(exact.len(), MAX_MAILBOX_NAME);
    let over = format!("{exact}a");
    assert!(Key::new(MailboxId::from_bytes([1; 16]), &exact, (1 << 31) - 1).is_ok());
    assert!(matches!(
        Key::new(MailboxId::from_bytes([1; 16]), &over, 0),
        Err(Error::InvalidKey)
    ));
    assert!(matches!(
        Key::new(MailboxId::from_bytes([1; 16]), "ok", 1 << 31),
        Err(Error::InvalidKey)
    ));
}
#[test]
fn comparison_is_total_and_transitive_on_mixed_keys() {
    let keys = [
        key(1, "A", 0),
        key(2, "a", 0),
        key(3, "[", 1),
        key(4, "_", 1),
        key(5, "\u{c9}", 0),
        key(6, "é", 2),
        key(7, "\u{130}", 0),
        key(8, "i", 0),
    ];
    for collation in [None, Some("i;octet"), Some("i;ascii-casemap")] {
        for ascending in [true, false] {
            let comparators = [
                Comparator::new("name", ascending, collation).unwrap(),
                Comparator::new("sortOrder", !ascending, None).unwrap(),
            ];
            let plan = Plan::new(&comparators).unwrap();
            for a in keys {
                assert_eq!(plan.compare(a, a).unwrap(), Ordering::Equal);
                for b in keys {
                    assert_eq!(
                        plan.compare(a, b).unwrap(),
                        plan.compare(b, a).unwrap().reverse()
                    );
                    for c in keys {
                        if plan.compare(a, b).unwrap() != Ordering::Greater
                            && plan.compare(b, c).unwrap() != Ordering::Greater
                        {
                            assert_ne!(plan.compare(a, c).unwrap(), Ordering::Greater);
                        }
                    }
                }
            }
        }
    }
}
