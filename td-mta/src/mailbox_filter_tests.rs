#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
fn id(value: u8) -> MailboxId {
    MailboxId::from_bytes([value; 16])
}
fn row() -> MailboxRow<'static> {
    MailboxRow {
        name: "My Inbox",
        parent: None,
        role: Some("inbox"),
        sort_order: 0,
        subscribed: true,
    }
}
fn matches(pattern: &str, name: &str) -> bool {
    let mut scalars = vec!['\0'; pattern.len()];
    let mut fallback = vec![0; pattern.len()];
    NamePattern::compile(pattern, &mut scalars, &mut fallback)
        .unwrap()
        .matches(name)
        .unwrap()
}
#[test]
fn literal_substrings_use_simple_lowercase_without_phrase_or_normalization() {
    for (pattern, name, expected) in [
        ("inBOX", "My Inbox", true),
        ("", "", true),
        ("", "Mailbox", true),
        ("a", "", false),
        ("two words", "one TWO WORDS three", true),
        ("two words", "words two", false),
        ("\"two words\"", "two words", false),
        ("a\\b", "A\\B", true),
        ("i", "\u{130}", true),
        ("k", "\u{212a}", true),
        ("é", "É", true),
        ("e\u{301}", "é", false),
        ("ss", "ß", false),
        ("\u{3c3}", "\u{3c2}", false),
        ("é", "e", false),
        ("aba", "aababa", true),
        ("猫猫犬", "猫猫猫猫犬", true),
        ("猫猫犬", "猫猫猫猫", false),
        ("😺", "x😺y", true),
        ("\u{378}", "x\u{378}y", true),
        ("ǰ", "J\u{30c}", false),
        ("ababac", "abababababac", true),
        ("ababac", "abababababa", false),
    ] {
        assert_eq!(matches(pattern, name), expected, "{pattern:?} in {name:?}");
    }
}
fn words(maximum: usize) -> Vec<String> {
    let mut values = vec![String::new()];
    for length in 1..=maximum {
        for mut code in 0..3usize.pow(length as u32) {
            let mut value = String::new();
            for _ in 0..length {
                value.push(['a', 'A', 'b'][code % 3]);
                code /= 3;
            }
            values.push(value);
        }
    }
    values
}
#[test]
fn every_small_ascii_pattern_matches_an_independent_naive_oracle() {
    let names = words(6);
    for pattern in words(4) {
        let mut scalars = ['\0'; 4];
        let mut fallback = [0; 4];
        let compiled = NamePattern::compile(&pattern, &mut scalars, &mut fallback).unwrap();
        let expected_pattern = pattern.to_ascii_lowercase();
        for name in &names {
            assert_eq!(
                compiled.matches(name).unwrap(),
                name.to_ascii_lowercase().contains(&expected_pattern),
                "{pattern:?} in {name:?}"
            );
        }
    }
}
#[test]
fn compilation_scratch_and_text_byte_limits_are_exact() {
    assert_eq!(NamePattern::scratch_len("ÉéA").unwrap(), 3);
    assert_eq!(
        NamePattern::scratch_len(&"a".repeat(MAX_TEXT_BYTES + 1)),
        Err(Error::UnsupportedFilter)
    );
    let mut scalars = ['!'; 4];
    let mut fallback = [usize::MAX; 4];
    {
        let pattern = NamePattern::compile("ÉéA", &mut scalars, &mut fallback).unwrap();
        assert!(pattern.matches("éÉa").unwrap());
        assert_eq!(pattern.scalars.len(), 3);
        assert_eq!(pattern.fallback, [0, 1, 0]);
    }
    assert_eq!(scalars[3], '!');
    assert_eq!(fallback[3], usize::MAX);
    for (chars, prefix) in [(2, 3), (3, 2), (0, 3), (3, 0)] {
        assert!(matches!(
            NamePattern::compile("ÉéA", &mut scalars[..chars], &mut fallback[..prefix]),
            Err(Error::Scratch)
        ));
    }
    assert!(NamePattern::compile("", &mut [], &mut [])
        .unwrap()
        .matches("x")
        .unwrap());
    let exact = "é".repeat(MAX_TEXT_BYTES / 2);
    let mut scalars = vec!['\0'; MAX_TEXT_BYTES];
    let mut fallback = vec![0; MAX_TEXT_BYTES];
    assert!(NamePattern::compile(&exact, &mut scalars, &mut fallback).is_ok());
    assert!(matches!(
        NamePattern::compile(&format!("{exact}x"), &mut scalars, &mut fallback),
        Err(Error::UnsupportedFilter)
    ));
    let ascii = "a".repeat(MAX_TEXT_BYTES);
    let pattern = NamePattern::compile(&ascii, &mut scalars, &mut fallback).unwrap();
    assert!(!pattern.matches(&"a".repeat(MAX_MAILBOX_NAME)).unwrap());
    assert_eq!(
        pattern.matches(&"a".repeat(MAX_MAILBOX_NAME + 1)),
        Err(Error::InvalidRow)
    );
    let name = format!("{}b", "a".repeat(MAX_MAILBOX_NAME - 1));
    assert!(matches(&format!("{}b", "a".repeat(511)), &name));
    assert!(!matches(&format!("{}c", "a".repeat(511)), &name));
}
#[test]
fn parent_ids_distinguish_omission_null_local_and_foreign() {
    let mut bytes = [0; 33];
    let local = wire::encode_id(id(0xab), &mut bytes).unwrap();
    for (parent, roots, same, other) in [
        (Parent::Any, true, true, true),
        (Parent::Root, true, false, false),
        (
            Parent::exact(wire::Id::parse(local).unwrap()),
            false,
            true,
            false,
        ),
        (
            Parent::exact(wire::Id::parse("foreign").unwrap()),
            false,
            false,
            false,
        ),
        (
            Parent::exact(wire::Id::parse(&local.to_ascii_uppercase()).unwrap()),
            false,
            false,
            false,
        ),
        (
            Parent::exact(wire::Id::parse("eabababababababababababababababab").unwrap()),
            false,
            false,
            false,
        ),
    ] {
        let condition = Condition {
            parent,
            ..Condition::default()
        };
        for (parent, expected) in [(None, roots), (Some(id(0xab)), same), (Some(id(1)), other)] {
            assert_eq!(
                condition.matches(MailboxRow { parent, ..row() }).unwrap(),
                expected
            );
        }
    }
}
#[test]
fn conditions_are_conjunctive_and_role_checks_are_exact() {
    let mut scalars = ['\0'; 5];
    let mut fallback = [0; 5];
    let pattern = NamePattern::compile("inbox", &mut scalars, &mut fallback).unwrap();
    for actual_parent in [None, Some(id(1)), Some(id(2))] {
        for actual_role in [None, Some("inbox"), Some("Inbox"), Some("archive")] {
            for subscribed in [false, true] {
                for name in ["My Inbox", "Sent"] {
                    let value = MailboxRow {
                        parent: actual_parent,
                        role: actual_role,
                        subscribed,
                        name,
                        ..row()
                    };
                    for (parent, parent_matches) in [
                        (Parent::Any, true),
                        (Parent::Root, actual_parent.is_none()),
                        (Parent::Local(id(1)), actual_parent == Some(id(1))),
                    ] {
                        for (role, role_matches) in [
                            (Role::Any, true),
                            (Role::Exact(None), actual_role.is_none()),
                            (Role::Exact(Some("inbox")), actual_role == Some("inbox")),
                            (Role::Exact(Some("Inbox")), actual_role == Some("Inbox")),
                            (Role::Exact(Some("unknown")), false),
                        ] {
                            for has_any_role in [None, Some(false), Some(true)] {
                                for wanted_subscribed in [None, Some(false), Some(true)] {
                                    for wanted_name in [None, Some(&pattern)] {
                                        let condition = Condition {
                                            parent,
                                            name: wanted_name,
                                            role,
                                            has_any_role,
                                            subscribed: wanted_subscribed,
                                        };
                                        let expected = parent_matches
                                            && role_matches
                                            && (has_any_role.is_none()
                                                || has_any_role == Some(actual_role.is_some()))
                                            && (wanted_subscribed.is_none()
                                                || wanted_subscribed == Some(subscribed))
                                            && (wanted_name.is_none() || name == "My Inbox");
                                        assert_eq!(condition.matches(value).unwrap(), expected);
                                    }
                                }
                            }
                        }
                    }
                    assert!(Condition::default().matches(value).unwrap());
                }
            }
        }
    }
    let condition = Condition {
        name: Some(&pattern),
        ..Condition::default()
    };
    assert!(!condition
        .matches(MailboxRow {
            name: "Sent",
            ..row()
        })
        .unwrap());
}
#[test]
fn row_bounds_are_checked_even_when_another_condition_would_not_match() {
    let condition = Condition {
        parent: Parent::NoLocal,
        ..Condition::default()
    };
    let name = "a".repeat(MAX_MAILBOX_NAME + 1);
    assert_eq!(
        condition.matches(MailboxRow {
            name: &name,
            ..row()
        }),
        Err(Error::InvalidRow)
    );
    let role = "a".repeat(MAX_ROLE + 1);
    assert_eq!(
        condition.matches(MailboxRow {
            role: Some(&role),
            ..row()
        }),
        Err(Error::InvalidRow)
    );
    let role = "a".repeat(MAX_ROLE);
    let name = "a".repeat(MAX_MAILBOX_NAME);
    assert!(Condition::default()
        .matches(MailboxRow {
            name: &name,
            role: Some(&role),
            ..row()
        })
        .unwrap());
}
