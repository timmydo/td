//! D-Bus object-path and name validation.
//!
//! Every rule here is the wire specification's. They are grouped in one module
//! because the four grammars differ in exactly the details a reader assumes are
//! shared: a hyphen is legal in a bus name and nowhere else, an element of a
//! UNIQUE bus name may begin with a digit where every other element may not,
//! and a member name is the only one of the four that may not contain a dot.

/// The specification's cap on interface, error, bus and member names.
///
/// Object paths carry no cap of their own; the message layer's header-field
/// ceiling is what bounds them, since a path only ever arrives as one.
pub const MAX_NAME_LEN: usize = 255;

fn element_chars_ok(element: &str, hyphen: bool) -> bool {
    element
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'_' || (hyphen && c == b'-'))
}

/// `[A-Za-z_]` followed by `[A-Za-z0-9_]*`, plus `-` where a bus name allows it.
fn element_ok(element: &str, hyphen: bool, leading_digit: bool) -> bool {
    match element.as_bytes().first() {
        None => false,
        Some(c) if c.is_ascii_digit() && !leading_digit => false,
        Some(_) => element_chars_ok(element, hyphen),
    }
}

fn dotted_ok(name: &str, hyphen: bool, leading_digit: bool) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return false;
    }
    let mut elements = 0usize;
    for element in name.split('.') {
        elements += 1;
        if !element_ok(element, hyphen, leading_digit) {
            return false;
        }
    }
    elements >= 2
}

/// `/`, or `/`-separated non-empty `[A-Za-z0-9_]` elements with no trailing `/`.
pub fn valid_object_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.first() != Some(&b'/') {
        return false;
    }
    if bytes.len() == 1 {
        return true;
    }
    if bytes.last() == Some(&b'/') {
        return false;
    }
    // `split` yields a leading "" for the root slash; every later element must
    // be non-empty, which is what refuses `//`.
    path.split('/')
        .skip(1)
        .all(|element| !element.is_empty() && element_chars_ok(element, false))
}

/// Two or more dot-separated elements, no hyphen, no leading digit.
pub fn valid_interface_name(name: &str) -> bool {
    dotted_ok(name, false, false)
}

/// Error names share the interface grammar exactly.
pub fn valid_error_name(name: &str) -> bool {
    valid_interface_name(name)
}

/// One element, no dot, no leading digit.
pub fn valid_member_name(name: &str) -> bool {
    if name.len() > MAX_NAME_LEN {
        return false;
    }
    element_ok(name, false, false)
}

/// `:` then two or more dot-separated elements, which MAY begin with a digit —
/// `:1.0` is the shape the broker itself hands out.
pub fn valid_unique_name(name: &str) -> bool {
    if name.len() > MAX_NAME_LEN {
        return false;
    }
    match name.strip_prefix(':') {
        Some(rest) => {
            let mut elements = 0usize;
            for element in rest.split('.') {
                elements += 1;
                if element.is_empty() || !element_chars_ok(element, true) {
                    return false;
                }
            }
            elements >= 2
        }
        None => false,
    }
}

/// A claimable name: dotted, hyphen allowed, no leading `:` and no leading digit.
pub fn valid_well_known_name(name: &str) -> bool {
    !name.starts_with(':') && dotted_ok(name, true, false)
}

/// Either spelling of a destination or sender.
pub fn valid_bus_name(name: &str) -> bool {
    valid_unique_name(name) || valid_well_known_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_paths_follow_the_specification() {
        for ok in [
            "/",
            "/a",
            "/org/freedesktop/DBus",
            "/org/freedesktop/portal/desktop",
            "/_",
            "/0",
            "/a1/B2/c_3",
        ] {
            assert!(valid_object_path(ok), "{ok} should be a valid path");
        }
        for bad in [
            "", "a", "a/b", "/a/", "//", "/a//b", "/a.b", "/a-b", "/a b", "/é", "/a/",
        ] {
            assert!(!valid_object_path(bad), "{bad:?} should be refused");
        }
    }

    #[test]
    fn interface_names_need_two_elements_and_no_leading_digit() {
        for ok in [
            "org.freedesktop.DBus",
            "a.b",
            "_._",
            "org.freedesktop.DBus.Properties",
        ] {
            assert!(valid_interface_name(ok), "{ok} should be valid");
        }
        for bad in [
            "",
            "org",
            ".org.freedesktop",
            "org.freedesktop.",
            "org..freedesktop",
            "org.1freedesktop",
            "org.free-desktop",
            "org.free desktop",
        ] {
            assert!(!valid_interface_name(bad), "{bad:?} should be refused");
        }
        assert!(valid_error_name("org.freedesktop.DBus.Error.Failed"));
    }

    #[test]
    fn member_names_are_one_element() {
        for ok in ["Hello", "_x", "AddMatch", "a0"] {
            assert!(valid_member_name(ok), "{ok} should be valid");
        }
        for bad in ["", "a.b", "0a", "a-b", "a b"] {
            assert!(!valid_member_name(bad), "{bad:?} should be refused");
        }
    }

    #[test]
    fn unique_names_may_start_an_element_with_a_digit_and_well_known_names_may_not() {
        assert!(valid_unique_name(":1.0"));
        assert!(valid_unique_name(":1.4294967295"));
        assert!(valid_bus_name(":1.0"));
        assert!(!valid_well_known_name(":1.0"));
        assert!(!valid_interface_name(":1.0"));

        assert!(valid_well_known_name("org.freedesktop.DBus"));
        assert!(valid_well_known_name("org.free-desktop.DBus"));
        assert!(valid_bus_name("org.freedesktop.DBus"));
        assert!(!valid_unique_name("org.freedesktop.DBus"));

        for bad in [":", ":1", ":1.", ":.1", ":1..2", ":1.a b"] {
            assert!(!valid_unique_name(bad), "{bad:?} should be refused");
        }
    }

    #[test]
    fn the_length_cap_counts_the_whole_name() {
        let long_element = "a".repeat(MAX_NAME_LEN - 2);
        let at_cap = format!("a.{long_element}");
        assert_eq!(at_cap.len(), MAX_NAME_LEN);
        assert!(valid_interface_name(&at_cap));
        assert!(!valid_interface_name(&format!("{at_cap}b")));

        // The colon is part of the name, so a unique name one byte longer than
        // its well-known twin is refused where the twin is accepted.
        let unique_at_cap = format!(":1.{}", "0".repeat(MAX_NAME_LEN - 3));
        assert_eq!(unique_at_cap.len(), MAX_NAME_LEN);
        assert!(valid_unique_name(&unique_at_cap));
        assert!(!valid_unique_name(&format!("{unique_at_cap}0")));

        assert!(valid_member_name(&"m".repeat(MAX_NAME_LEN)));
        assert!(!valid_member_name(&"m".repeat(MAX_NAME_LEN + 1)));
    }
}
