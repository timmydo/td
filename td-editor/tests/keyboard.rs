#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use td_editor::keyboard::{InputError, Keymap, Modifiers};

const US: &str = include_str!("fixtures/us.xkb");

#[test]
fn ordinary_us_key_selection_matches_independent_libxkbcommon_oracle() {
    let map = Keymap::parse(US).unwrap();
    let mut count = 0;
    for line in include_str!("fixtures/us-keys.tsv").lines() {
        let (code, digest) = line.split_once('\t').unwrap();
        let code: u32 = code.parse().unwrap();
        let mut hash = 0xcbf29ce484222325u64;
        for mask in 0..32 {
            let key = map.lookup(code - 8, state(mask)).unwrap().unwrap();
            if key.keysym.is_none() {
                assert!(key.name.starts_with("XF86"), "{code}: {}", key.name);
                assert_eq!(chord(&map, code - 8, mask), None);
            }
            for value in [
                key.level as u32,
                key.keysym.unwrap_or(u32::MAX),
                key.consumed,
                u32::from(key.repeat),
            ] {
                for byte in value.to_le_bytes() {
                    hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                }
            }
        }
        assert_eq!(format!("{hash:016x}"), digest, "XKB keycode {code}");
        count += 1;
    }
    assert_eq!(count, 106);
}

fn td() -> &'static str {
    include_str!("../../td-compositor/src/keyboard.rs")
        .split_once("pub const XKB_KEYMAP: &str = r#\"")
        .expect("td map declaration changed")
        .1
        .split_once("\"#;")
        .expect("td map delimiter changed")
        .0
}

fn state(mask: u32) -> Modifiers {
    Modifiers {
        depressed: mask,
        ..Modifiers::default()
    }
}
fn chord(map: &Keymap, code: u32, mask: u32) -> Option<String> {
    map.translate(code, state(mask))
        .unwrap()
        .map(|stroke| stroke.chord)
}

#[test]
fn supplied_td_and_ordinary_us_maps_translate_editor_keys() {
    for source in [td(), US] {
        let map = Keymap::parse(source).unwrap();
        for (code, mask, expected) in [
            (30, 0, "a"),
            (30, 1, "A"),
            (30, 2, "A"),
            (30, 3, "a"),
            (31, 4, "C-s"),
            (31, 6, "C-s"),
            (31, 5, "C-S-s"),
            (31, 7, "C-S-s"),
            (16, 8, "M-q"),
            (57, 4, "C-Space"),
            (28, 0, "Return"),
            (14, 0, "Backspace"),
            (105, 1, "S-Left"),
            (106, 5, "C-S-Right"),
            (15, 5, "C-S-Tab"),
            (65, 0, "F7"),
        ] {
            assert_eq!(
                chord(&map, code, mask).as_deref(),
                Some(expected),
                "code={code} mask={mask}"
            );
        }
        assert_eq!(chord(&map, 29, 4), None);
        assert!(map.translate(30, state(64)).is_err());
        assert!(map.translate(30, state(128)).is_err());
        assert!(map
            .translate(
                30,
                Modifiers {
                    group: 1,
                    ..state(0)
                }
            )
            .is_err());
        assert!(map.translate(u32::MAX, state(0)).unwrap().is_none());
    }
}

#[test]
fn us_virtual_bindings_and_repeat_come_from_interpretations() {
    let map = Keymap::parse(US).unwrap();
    for (name, expected) in [
        ("NumLock", 16),
        ("Alt", 8),
        ("Meta", 8),
        ("Super", 64),
        ("Hyper", 32),
        ("LevelThree", 128),
        ("LevelFive", 32),
        ("ScrollLock", 32768),
    ] {
        assert_eq!(map.virtual_mask(name), Some(expected), "{name}");
    }
    for code in [29, 42, 54, 56, 58, 69, 97, 100, 125, 126] {
        assert!(
            !map.lookup(code, state(0)).unwrap().unwrap().repeat,
            "{code}"
        );
    }
    for code in [1, 14, 15, 16, 28, 30, 59, 79, 105] {
        assert!(
            map.lookup(code, state(0)).unwrap().unwrap().repeat,
            "{code}"
        );
    }
}

#[test]
fn numlock_and_function_levels_are_selected_not_assumed() {
    let map = Keymap::parse(US).unwrap();
    assert_eq!(chord(&map, 79, 0).as_deref(), Some("End"));
    assert_eq!(chord(&map, 79, 16).as_deref(), Some("1"));
    assert_eq!(chord(&map, 79, 17).as_deref(), Some("End"));
    assert_eq!(chord(&map, 59, 1).as_deref(), Some("S-F1"));
    assert_eq!(chord(&map, 59, 12), None);
    assert_eq!(
        chord(&Keymap::parse(td()).unwrap(), 79, 0).as_deref(),
        Some("1")
    );
}

fn small(codes: &str, keys: &str, compatibility: &str, types: &str) -> String {
    format!(
        r#"xkb_keymap {{
        xkb_keycodes {{ <SHIFT>=200; <CAPS>=201; <CTRL>=202; <ALT>=203; <NUM>=204; {codes} }};
        xkb_types {{ virtual_modifiers NumLock,V,W;
            type "ONE_LEVEL" {{ modifiers=none; }};
            type "TWO_LEVEL" {{ modifiers=Shift; map[Shift]=2; }};
            type "ALPHABETIC" {{ modifiers=Shift+Lock; map[Shift]=2; map[Lock]=2; }};
            type "KEYPAD" {{ modifiers=Shift+NumLock; map[NumLock]=2; }};
            type "FOUR_LEVEL" {{ modifiers=Shift; map[Shift]=2; level_name[4]="Fourth"; }};
            {types}
        }};
        xkb_compatibility {{
            interpret Num_Lock+AnyOf(all) {{ virtualModifier=NumLock; action=LockMods(modifiers=NumLock); }};
            {compatibility}
        }};
        xkb_symbols {{
            key <SHIFT> {{ [Shift_L] }}; key <CAPS> {{ [Caps_Lock] }};
            key <CTRL> {{ [Control_L] }}; key <ALT> {{ [Alt_L] }}; key <NUM> {{ [Num_Lock] }};
            modifier_map Shift {{ <SHIFT> }}; modifier_map Lock {{ <CAPS> }};
            modifier_map Control {{ <CTRL> }}; modifier_map Mod1 {{ <ALT> }}; modifier_map Mod2 {{ <NUM> }};
            {keys}
        }};
    }};"#
    )
}

#[test]
fn aliases_and_relocated_keycodes_have_no_physical_us_fallback() {
    let source = small(
        "<REAL>=9; alias <FIRST>=<SECOND>; alias <SECOND>=<REAL>;",
        "key <FIRST> { [z,Z] };",
        "",
        "",
    );
    let map = Keymap::parse(&source).unwrap();
    assert_eq!(chord(&map, 1, 0).as_deref(), Some("z"));
    assert_eq!(chord(&map, 1, 1).as_deref(), Some("Z"));
    assert_eq!(chord(&map, 44, 0), None);
    let map = Keymap::parse(&source.replace("<REAL>=9", "<REAL>=700")).unwrap();
    assert_eq!(chord(&map, 692, 0).as_deref(), Some("z"));
    assert_eq!(chord(&map, 1, 0), None);
}

#[test]
fn shortcut_masks_follow_assignments_and_modifier_action_operands() {
    let source = small("<A>=9;", "key <A> { [a,A] };", "", "");
    let moved = source
        .replace("modifier_map Control", "modifier_map Mod3")
        .replace("modifier_map Mod1", "modifier_map Mod4");
    let map = Keymap::parse(&moved).unwrap();
    assert_eq!(chord(&map, 1, 32).as_deref(), Some("C-a"));
    assert_eq!(chord(&map, 1, 64).as_deref(), Some("M-a"));
    assert!(map.translate(1, state(4)).is_err());
    let actions = small("<A>=9;", "key <A> { [a,A] };",
        "interpret Control_L { action=SetMods(modifiers=Mod3); }; interpret Alt_L { action=LatchMods(modifiers=Mod4); };", "");
    let map = Keymap::parse(&actions).unwrap();
    assert_eq!(chord(&map, 1, 32).as_deref(), Some("C-a"));
    assert_eq!(chord(&map, 1, 64).as_deref(), Some("M-a"));
    let overlap = actions.replace("modifiers=Mod4", "modifiers=Mod3");
    assert!(Keymap::parse(&overlap)
        .unwrap_err()
        .reason
        .contains("ambiguous"));
}

#[test]
fn indirect_modifier_map_uses_lowest_level_then_lowest_keycode() {
    let source = small("<A>=9; <B>=10; <C>=11;",
        "key <A> { [NoSymbol,F13] }; key <B> { [F13] }; key <C> { [F13] }; modifier_map Mod3 { F13 };",
        "interpret F13+AnyOf(all) { virtualModifier=V; };", "");
    let map = Keymap::parse(&source).unwrap();
    assert_eq!(map.virtual_mask("V"), Some(32));
    // NoSymbol never repeats implicitly; only B matches the false interpret.
    assert!(!map.lookup(1, state(0)).unwrap().unwrap().repeat);
    assert!(!map.lookup(2, state(0)).unwrap().unwrap().repeat);
    assert!(map.lookup(3, state(0)).unwrap().unwrap().repeat);
}

#[test]
fn interpret_specificity_predicates_and_first_ties_are_deterministic() {
    for (interpretations, expected) in [
        ("interpret Any+Exactly(Mod3) { virtualModifier=W; }; interpret F13+AnyOfOrNone(all) { virtualModifier=V; };", "V"),
        ("interpret F13+AnyOf(all) { virtualModifier=W; }; interpret F13+Exactly(Mod3) { virtualModifier=V; };", "V"),
        ("interpret F13+AnyOf(all) { virtualModifier=V; }; interpret F13+AnyOf(Mod3) { virtualModifier=W; };", "V"),
        ("interpret F13+NoneOf(Control) { virtualModifier=V; }; interpret F13+AnyOf(all) { virtualModifier=W; };", "V"),
        ("interpret F13+AllOf(Mod3) { virtualModifier=V; }; interpret F13+NoneOf(Control) { virtualModifier=W; };", "V"),
        ("interpret F13+Mod3 { virtualModifier=V; };", "V"),
    ] {
        let map = Keymap::parse(&small("<A>=9;", "key <A> { [F13] }; modifier_map Mod3 { <A> };", interpretations, "")).unwrap();
        assert_eq!(map.virtual_mask(expected), Some(32), "{interpretations}");
        assert_eq!(map.virtual_mask("W"), Some(0), "{interpretations}");
    }
}

#[test]
fn repeat_and_explicit_virtual_maps_override_interpretations() {
    let base = small(
        "<A>=9;",
        "key <A> { repeat=yes, virtualModifiers=W, [F13] }; modifier_map Mod3 { <A> };",
        "interpret F13+Any { repeat=no; virtualModifier=V; };",
        "",
    );
    let map = Keymap::parse(&base).unwrap();
    assert!(map.lookup(1, state(0)).unwrap().unwrap().repeat);
    assert_eq!(map.virtual_mask("V"), Some(0));
    assert_eq!(map.virtual_mask("W"), Some(32));
    let map = Keymap::parse(&base.replace("virtualModifiers=W", "virtualModifiers=none")).unwrap();
    assert_eq!(map.virtual_mask("V"), Some(0));
    assert_eq!(map.virtual_mask("W"), Some(0));
    let explicit = base.replace("NumLock,V,W", "NumLock,V,W=0x8000");
    assert_eq!(
        Keymap::parse(&explicit).unwrap().virtual_mask("W"),
        Some(0x8020)
    );
}

#[test]
fn no_symbol_has_no_interpretation_or_implicit_repeat() {
    // Independently checked with libxkbcommon 1.13.1: an empty first level
    // does not match even an Any interpretation and cannot infer repeat.
    for symbols in ["NoSymbol", "NoSymbol,F13", "NoSymbol,a"] {
        let source = small(
            "<A>=9;",
            &format!("key <A> {{ [{symbols}] }}; modifier_map Mod3 {{ <A> }};"),
            "interpret Any { repeat=true; virtualModifier=V; useModMapMods=level1; };",
            "",
        );
        let map = Keymap::parse(&source).unwrap();
        assert!(!map.lookup(1, state(0)).unwrap().unwrap().repeat);
        // The four baseline shortcut keys contribute 1|2|4|8, but A does not.
        assert_eq!(map.virtual_mask("V"), Some(15));
        let explicit = source.replace("key <A> {", "key <A> { repeat=yes,");
        assert!(
            Keymap::parse(&explicit)
                .unwrap()
                .lookup(1, state(0))
                .unwrap()
                .unwrap()
                .repeat
        );
    }
}

#[test]
fn duplicate_interpretations_are_refused_instead_of_guessing_merge_semantics() {
    for symbol in ["a", "U0061", "0x61"] {
        let source = small("<A>=9;", "key <A> { [a] };",
            &format!("interpret a+Any {{ repeat=no; }}; interpret {symbol}+AnyOf(all) {{ repeat=yes; }};"), "");
        assert!(Keymap::parse(&source)
            .unwrap_err()
            .reason
            .contains("duplicate interpretation"));
    }
}

#[test]
fn defaults_apply_in_order_and_level_one_only_ignores_later_levels() {
    let map = Keymap::parse(&small("<A>=9; <B>=10;",
        "key.type=\"TWO_LEVEL\"; key.repeat=no; key <A> { [a,A] }; key.repeat=yes; key <B> { [NoSymbol,F13] }; modifier_map Mod3 { <B> };",
        "interpret.useModMapMods=level1; interpret.repeat=False; interpret F13+AnyOf(all) { virtualModifier=V; };", "")).unwrap();
    assert_eq!(map.virtual_mask("V"), Some(0));
    assert!(!map.lookup(1, state(0)).unwrap().unwrap().repeat);
    assert!(map.lookup(2, state(1)).unwrap().unwrap().repeat);
    assert_eq!(chord(&map, 1, 3).as_deref(), Some("A")); // unconsumed Lock
}

#[test]
fn explicit_type_truncation_precedes_interpretation_matching() {
    let map = Keymap::parse(&small(
        "<A>=9;",
        "key <A> { type=\"ONE_LEVEL\", [NoSymbol,F13] }; modifier_map Mod3 { <A> };",
        "interpret F13+Any { virtualModifier=V; };",
        "",
    ))
    .unwrap();
    assert_eq!(map.virtual_mask("V"), Some(0));
    assert_eq!(map.lookup(1, state(1)).unwrap().unwrap().keysym, Some(0));
    // Inference chooses a type name once; resizing to that type does not infer again.
    let source = small("<A>=9;", "key <A> { [a,b] };", "", "").replace(
        "type \"TWO_LEVEL\" { modifiers=Shift; map[Shift]=2; }",
        "type \"TWO_LEVEL\" { modifiers=Control; preserve[Control]=Control; }",
    );
    assert_eq!(
        Keymap::parse(&source)
            .unwrap()
            .lookup(1, state(0))
            .unwrap()
            .unwrap()
            .consumed,
        4
    );
}

#[test]
fn consumed_shortcut_modifiers_and_preservation_change_chord_meaning() {
    let make = |preserve| {
        small(
            "<A>=9;",
            "key <A> { type=\"custom\", [a,b] };",
            "",
            &format!("type \"custom\" {{ modifiers=Control; map[Control]=2; {preserve} }};"),
        )
    };
    assert_eq!(
        chord(&Keymap::parse(&make("")).unwrap(), 1, 4).as_deref(),
        Some("b")
    );
    assert_eq!(
        chord(
            &Keymap::parse(&make("preserve[Control]=Control;")).unwrap(),
            1,
            4
        )
        .as_deref(),
        Some("C-b")
    );
}

#[test]
fn unsupported_maps_fail_before_any_key_event() {
    let base = small("<A>=9;", "key <A> { [a,A] };", "", "");
    for invalid in [
        base.replace("[a,A]", "[a,A], [b,B]"),
        base.replace("[a,A]", "symbols[Group2]=[a,A]"),
        base.replace("[a,A]", "type[Group2]=\"TWO_LEVEL\", [a,A]"),
        base.replace("[a,A]", "[dead_acute,A]"),
        base.replace("[a,A]", "[U0430,A]"),
        base.replace("[a,A]", "[a, { A, B } ]"),
        base.replace("[a,A]", "actions[1]=[NoAction()], [a,A]"),
        base.replace("[a,A]", "vmodmap=V, [a,A]"),
        base.replace("[a,A]", "type=\"absent\", [a,A]"),
        base.replace("[a,A]", "[a,b,c,d,e]"),
        base.replace("<A>=9;", "<A>=9; alias <LOOP>=<LOOP>;"),
        base.replace("<A>=9;", "<A>=9; alias <BAD>=<ABSENT>;"),
        base.replace("<A>=9;", "<A>=9; alias <A>=<SHIFT>;"),
        base.replace("<A>=9;", "<A>=9; <ALSO>=9;"),
        base.replace("<A>=9;", "minimum=10; <A>=9;"),
        base.replace("<A>=9;", "maximum=100; <A>=9;"),
        base.replace("modifier_map Shift", "modifier_map Mod1"),
        base.replace(
            "modifier_map Mod1 { <ALT> };",
            "modifier_map Mod1 { <ALT> }; modifier_map Control { <ALT> };",
        ),
        base.replace("key <A>", "key <MISSING>"),
    ] {
        assert!(Keymap::parse(&invalid).is_err(), "{invalid}");
    }
    for action in [
        "RedirectKey(key=<A>)",
        "{ NoAction(), RedirectKey(key=<A>) }",
    ] {
        let source = small(
            "<A>=9;",
            "key <A> { [a,A] };",
            &format!("interpret XF86Unused {{ action={action}; }};"),
            "",
        );
        assert!(Keymap::parse(&source)
            .unwrap_err()
            .reason
            .contains("redirect"));
    }
}

#[test]
fn unused_properties_and_symbols_never_become_physical_us_text() {
    let source = small(
        "<A>=9; <B>=10;",
        "key <A> { [a,A] }; key <B> { exotic=7, [XF86AudioMute] };",
        "",
        "",
    );
    let map = Keymap::parse(&source).unwrap();
    assert_eq!(chord(&map, 2, 0), None);
    assert_eq!(chord(&map, 1, 0).as_deref(), Some("a"));
    let map = Keymap::parse(&source.replace("XF86AudioMute", "Multi_key")).unwrap();
    assert!(
        matches!(map.translate(2, state(0)), Err(InputError::UnsupportedSymbol(d)) if d.item.contains("Multi_key"))
    );
}

#[test]
fn modifier_snapshots_union_without_mutating_map_and_profiles_share_chords() {
    let map = Keymap::parse(US).unwrap();
    let modifiers = Modifiers {
        depressed: 1,
        latched: 4,
        locked: 2,
        group: 0,
    };
    assert_eq!(
        map.translate(31, modifiers).unwrap().unwrap().chord,
        "C-S-s"
    );
    let mut profile = td_editor::keys::Keymap::default();
    assert!(matches!(
        profile.translate(&chord(&map, 31, 4).unwrap()).unwrap(),
        td_editor::keys::Action::Request("save")
    ));
    profile.set_profile(td_editor::keys::Profile::Emacs);
    assert!(matches!(
        profile.translate(&chord(&map, 45, 4).unwrap()).unwrap(),
        td_editor::keys::Action::Prefix
    ));
    assert!(matches!(
        profile.translate(&chord(&map, 31, 4).unwrap()).unwrap(),
        td_editor::keys::Action::Request("save")
    ));
}

#[test]
fn key_name_interpretation_and_symbol_level_limits_are_bounded() {
    let codes = (0..763)
        .map(|n| format!("<K{n}>={};", 1000 + n))
        .collect::<String>();
    assert!(Keymap::parse(&small(&codes, "", "", "")).is_ok());
    assert!(Keymap::parse(&small(&(codes + "<OVER>=3000;"), "", "", ""))
        .unwrap_err()
        .reason
        .contains("768"));
    let interpretations = (0..1023)
        .map(|n| format!("interpret 0x{:x} {{ repeat=no; }};", 0x1000 + n))
        .collect::<String>();
    assert!(Keymap::parse(&small("", "", &interpretations, "")).is_ok());
    assert!(Keymap::parse(&small(
        "",
        "",
        &(interpretations + "interpret 0x2000 {};"),
        ""
    ))
    .unwrap_err()
    .reason
    .contains("1024"));
    for count in [16, 17] {
        let symbols = std::iter::repeat_n("a", count)
            .collect::<Vec<_>>()
            .join(",");
        let source = small(
            "<A>=9;",
            &format!("key <A> {{ type=\"custom\", [{symbols}] }};"),
            "",
            "type \"custom\" { level_name[16]=\"Last\"; };",
        );
        assert_eq!(Keymap::parse(&source).is_ok(), count == 16);
    }
}

#[test]
fn malformed_byte_mutations_and_truncations_are_results() {
    let source = small("<A>=9;", "key <A> { [a,A] };", "", "");
    for end in (0..source.len()).step_by(7) {
        let _ = Keymap::parse(&source[..end]);
    }
    for offset in (0..source.len()).step_by(7) {
        for byte in [0, b'"', b'=', b'[', b'}', b';', b'+'] {
            let mut bytes = source.as_bytes().to_vec();
            bytes[offset] = byte;
            if let Ok(map) = Keymap::parse(&String::from_utf8(bytes).unwrap()) {
                for mask in 0..32 {
                    let _ = map.translate(1, state(mask));
                }
            }
        }
    }
}

#[test]
fn unknown_codes_are_ignored_before_state_admission_and_event_errors_are_typed() {
    let map = Keymap::parse(US).unwrap();
    for modifiers in [
        state(128),
        Modifiers {
            group: 3,
            ..state(0)
        },
    ] {
        for code in [u32::MAX, 0] {
            assert_eq!(map.lookup(code, modifiers).unwrap(), None);
            assert_eq!(map.translate(code, modifiers).unwrap(), None);
        }
        assert!(matches!(
            map.translate(30, modifiers),
            Err(InputError::UnsupportedState(_))
        ));
    }
    let mut count = 0;
    for code in map.keycodes() {
        assert!(map.lookup(code, state(0)).unwrap().is_some());
        for mask in 0..32 {
            match map.translate(code, state(mask)) {
                Ok(_) => {}
                Err(InputError::UnsupportedSymbol(_)) => count += 1,
                Err(error) => panic!("unexpected state failure: {error}"),
            }
        }
    }
    assert_eq!(count, 27 * 32);
    assert_eq!(chord(&map, 30, 0).as_deref(), Some("a"));
}

#[test]
fn indirect_modmap_search_uses_normalized_symbol_levels() {
    // libxkbcommon's V=32: A's F13 is truncated, so B owns the assignment.
    let map = Keymap::parse(&small("<A>=9; <B>=10;",
        "key <A> { type=\"ONE_LEVEL\", [a,F13] }; key <B> { [NoSymbol,F13] }; modifier_map Mod3 { F13 };",
        "interpret F13+Any { virtualModifier=V; };", "")).unwrap();
    assert_eq!(map.virtual_mask("V"), Some(32));
}

#[test]
fn level_one_predicates_use_zero_modmap_at_higher_levels_but_do_not_bind_virtuals() {
    // Independent libxkbcommon update_key probe: shifted A sets Mod3 (or
    // Mod1 for modMapMods), but V stays unbound with useModMapMods=level1.
    for (operand, mask) in [("Mod3", 32), ("modMapMods", 8)] {
        for predicate in ["AnyOfOrNone(all)", "NoneOf(Control)", "Exactly(none)"] {
            let source = small("<A>=9; <B>=10;",
                "key <A> { [NoSymbol,Meta_L] }; key <B> { [a,A] }; modifier_map Mod1 { <A> };",
                &format!("interpret Meta_L+{predicate} {{ useModMapMods=level1; virtualModifier=V; action=SetMods(modifiers={operand}); }};"), "");
            let map = Keymap::parse(&source).unwrap();
            assert_eq!(map.virtual_mask("V"), Some(0));
            assert_eq!(chord(&map, 2, mask).as_deref(), Some("M-a"));
            assert!(Keymap::parse(
                &source.replace("virtualModifier=V;", "unsupported=1; virtualModifier=V;")
            )
            .is_err());
        }
    }
}

#[test]
fn reference_probes_pin_trailing_no_symbol_inference_predicates_and_real_lock() {
    let trailing = Keymap::parse(&small("<A>=9;", "key <A> { [z,NoSymbol] };", "", "")).unwrap();
    assert_eq!(chord(&trailing, 1, 1).as_deref(), Some("z"));
    assert_eq!(trailing.lookup(1, state(1)).unwrap().unwrap().consumed, 0);
    let swapped = Keymap::parse(&small("<A>=9;", "key <A> { [A,a] };", "", "")).unwrap();
    for (mask, symbol) in [(0, 'A'), (1, 'a'), (2, 'A'), (3, 'A')] {
        let key = swapped.lookup(1, state(mask)).unwrap().unwrap();
        assert_eq!(key.keysym, Some(symbol as u32));
        assert_eq!(key.consumed, 1);
    }
    let none = Keymap::parse(&small(
        "<A>=9;",
        "key <A> { [F13] }; modifier_map Control { <A> };",
        "interpret F13+AnyOfOrNone(Shift) { virtualModifier=V; };",
        "",
    ))
    .unwrap();
    assert_eq!(none.virtual_mask("V"), Some(0));
    let source = small(
        "<A>=9; <B>=10;",
        "key <A> { [a] }; key <B> { type=\"caps\", [a,A] };",
        "interpret Caps_Lock { action=LockMods(modifiers=Mod3); };",
        "type \"caps\" { modifiers=Mod3; map[Mod3]=2; };",
    );
    let map = Keymap::parse(&source).unwrap();
    assert_eq!(chord(&map, 1, 32).as_deref(), Some("a"));
    assert_eq!(chord(&map, 2, 32).as_deref(), Some("A"));
    let all = small(
        "<A>=9;",
        "key <A> { [a] };",
        "interpret a { action=SetMods(modifiers=all); };",
        "",
    );
    assert_eq!(
        chord(&Keymap::parse(&all).unwrap(), 1, 0).as_deref(),
        Some("a")
    );
}
