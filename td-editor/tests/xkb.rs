#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use td_editor::xkb::{Selection, TypeCatalog, VirtualBinding};

const US: &str = include_str!("fixtures/us.xkb");
const BINDINGS: &[VirtualBinding<'_>] = &[
    VirtualBinding {
        name: "NumLock",
        mask: 16,
    },
    VirtualBinding {
        name: "Alt",
        mask: 8,
    },
    VirtualBinding {
        name: "LevelThree",
        mask: 128,
    },
    VirtualBinding {
        name: "Super",
        mask: 64,
    },
    VirtualBinding {
        name: "LevelFive",
        mask: 32,
    },
    VirtualBinding {
        name: "Meta",
        mask: 8,
    },
    VirtualBinding {
        name: "Hyper",
        mask: 32,
    },
];

fn map(types: &str) -> String {
    format!("xkb_keymap {{ xkb_keycodes {{}}; xkb_types {{ {types} }}; xkb_compatibility {{}}; xkb_symbols {{}}; }};")
}

fn table(body: &str) -> TypeCatalog {
    TypeCatalog::parse(&map(&format!("type \"custom\" {{ {body} }};"))).unwrap()
}

#[test]
fn ordinary_us_catalog_and_all_real_modifier_combinations() {
    let catalog = TypeCatalog::parse(US).unwrap();
    assert_eq!(catalog.names().count(), 26);
    for name in catalog.names() {
        let typ = catalog.resolve(name, BINDINGS).unwrap();
        for mask in 0..=255 {
            assert!(typ.select(mask).level < typ.levels(), "{name} mask={mask}");
        }
    }
    let alpha = catalog.resolve("ALPHABETIC", BINDINGS).unwrap();
    let keypad = catalog.resolve("KEYPAD", BINDINGS).unwrap();
    let function = catalog.resolve("CTRL+ALT", BINDINGS).unwrap();
    for mask in 0..=255 {
        assert_eq!(
            alpha.select(mask),
            Selection {
                level: usize::from(matches!(mask & 3, 1 | 2)),
                consumed: 3,
            }
        );
        assert_eq!(
            keypad.select(mask),
            Selection {
                level: usize::from(mask & 17 == 16),
                consumed: 17,
            }
        );
        let (level, preserve) = match mask & 141 {
            1 => (1, 1),
            128 => (2, 0),
            129 => (3, 1),
            12 => (4, 0),
            _ => (0, 0),
        };
        assert_eq!(
            function.select(mask),
            Selection {
                level,
                consumed: 141 & !preserve
            }
        );
    }
}

#[test]
fn all_us_type_results_match_the_libxkbcommon_oracle() {
    let catalog = TypeCatalog::parse(US).unwrap();
    let oracle = include_str!("fixtures/us-types.tsv");
    assert_eq!(oracle.lines().count(), catalog.names().count());
    let mut names = std::collections::BTreeSet::new();
    for line in oracle.lines() {
        let (name, expected) = line.split_once('\t').unwrap();
        assert!(names.insert(name));
        let typ = catalog.resolve(name, BINDINGS).unwrap();
        let mut hash = 0xcbf29ce484222325u64;
        for state in 0..256 {
            let selection = typ.select(state);
            for value in [selection.level as u32, selection.consumed] {
                for byte in value.to_le_bytes() {
                    hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                }
            }
        }
        assert_eq!(format!("{hash:016x}"), expected, "{name}");
    }
}

#[test]
fn td_map_is_parsed_by_meaning_not_serialized_equality() {
    let source = include_str!("../../td-compositor/src/keyboard.rs");
    let td = source
        .split_once("pub const XKB_KEYMAP: &str = r#\"")
        .expect("td-compositor keyboard.rs changed the XKB_KEYMAP raw-string declaration")
        .1
        .split_once("\"#;")
        .expect("td-compositor XKB_KEYMAP raw-string closing delimiter is missing")
        .0;
    let catalog = TypeCatalog::parse(td).unwrap();
    assert_eq!(catalog.names().count(), 3);
    let alpha = catalog.resolve("ALPHABETIC", &[]).unwrap();
    for state in 0..256 {
        assert_eq!(
            alpha.select(state).level,
            usize::from(matches!(state & 3, 1 | 2))
        );
    }
    let reformatted = td
        .replace("ALPHABETIC", "arbitrary renamed type")
        .replace("Shift+Lock", "Lock /* sum */ + Shift");
    let renamed = TypeCatalog::parse(&reformatted)
        .unwrap()
        .resolve("arbitrary renamed type", &[])
        .unwrap();
    for state in 0..256 {
        assert_eq!(alpha.select(state), renamed.select(state));
    }
}

#[test]
fn modifier_encodings_are_supplied_not_guessed() {
    let catalog = TypeCatalog::parse(US).unwrap();
    let moved = catalog
        .resolve(
            "KEYPAD",
            &[VirtualBinding {
                name: "NumLock",
                mask: 64,
            }],
        )
        .unwrap();
    assert_eq!(moved.select(16).level, 0);
    assert_eq!(moved.select(64).level, 1);
    assert_eq!(moved.select(65).level, 0);
    let function = catalog
        .resolve(
            "CTRL+ALT",
            &[VirtualBinding {
                name: "Alt",
                mask: 32,
            }],
        )
        .unwrap();
    assert_eq!(function.select(36).level, 4);
    assert_eq!(function.select(12).level, 0);
}

#[test]
fn unbound_virtual_entries_are_inactive_not_base_mappings() {
    let catalog = TypeCatalog::parse(&map("virtual_modifiers V; type \"custom\" { modifiers=Shift+V; map[V]=2; map[Shift+V]=3; map[Shift]=4; }; ")).unwrap();
    let typ = catalog.resolve("custom", &[]).unwrap();
    assert_eq!(
        typ.select(0),
        Selection {
            level: 0,
            consumed: 1
        }
    );
    assert_eq!(
        typ.select(1),
        Selection {
            level: 3,
            consumed: 1
        }
    );
    let bound = catalog
        .resolve(
            "custom",
            &[VirtualBinding {
                name: "V",
                mask: 0x8000,
            }],
        )
        .unwrap();
    assert_eq!(bound.select(0x8000).level, 1);
    assert_eq!(bound.select(0x8001).level, 2);
}

#[test]
fn preserve_masks_and_implicit_level_one_entries() {
    let typ = table(
        "modifiers=Shift+Control; map[Shift]=2; preserve[Shift]=Shift; preserve[Control]=Control;",
    )
    .resolve("custom", &[])
    .unwrap();
    assert_eq!(
        typ.select(1),
        Selection {
            level: 1,
            consumed: 4
        }
    );
    assert_eq!(
        typ.select(4),
        Selection {
            level: 0,
            consumed: 1
        }
    );
    assert_eq!(
        typ.select(5),
        Selection {
            level: 0,
            consumed: 5
        }
    );
    let typ = table("modifiers=Shift+Lock; map[Shift]=2; preserve[Shift]=Lock;")
        .resolve("custom", &[])
        .unwrap();
    assert_eq!(typ.select(1).consumed, 1);
}

#[test]
fn explicit_virtual_encodings_and_binding_diagnostics() {
    let catalog = TypeCatalog::parse(&map(
        "virtual_modifiers V=0x8000,W; type \"custom\" { modifiers=V+W; map[V]=2; map[W]=3; }; ",
    ))
    .unwrap();
    assert_eq!(
        catalog.resolve("custom", &[]).unwrap().select(0x8000).level,
        1
    );
    assert!(catalog
        .resolve("custom", &[VirtualBinding { name: "V", mask: 1 }])
        .unwrap_err()
        .reason
        .contains("explicit"));
    assert_eq!(
        catalog
            .resolve(
                "custom",
                &[VirtualBinding {
                    name: "W",
                    mask: 0x8000
                }]
            )
            .unwrap()
            .select(0x8000)
            .level,
        1
    );
    assert!(catalog
        .resolve(
            "custom",
            &[VirtualBinding {
                name: "unknown",
                mask: 1
            }]
        )
        .is_err());
    assert!(catalog
        .resolve(
            "custom",
            &[
                VirtualBinding { name: "W", mask: 1 },
                VirtualBinding { name: "W", mask: 1 }
            ]
        )
        .is_err());
}

#[test]
fn aliased_virtual_entries_use_declaration_order_not_modifier_order() {
    for (entries, expected) in [
        (
            "map[Alt]=2; map[Meta]=3;",
            Selection {
                level: 1,
                consumed: 8,
            },
        ),
        (
            "map[Meta]=3; map[Alt]=2;",
            Selection {
                level: 2,
                consumed: 8,
            },
        ),
        (
            "preserve[Alt]=Alt; map[Meta]=3;",
            Selection {
                level: 0,
                consumed: 0,
            },
        ),
        (
            "map[Meta]=3; preserve[Alt]=Alt;",
            Selection {
                level: 2,
                consumed: 8,
            },
        ),
        (
            "preserve[Alt]=Alt; map[Meta]=3; map[Alt]=2;",
            Selection {
                level: 1,
                consumed: 0,
            },
        ),
    ] {
        let catalog = TypeCatalog::parse(&map(&format!(
            "virtual_modifiers Alt,Meta; type \"custom\" {{ modifiers=Alt+Meta; {entries} }};"
        )))
        .unwrap();
        let typ = catalog
            .resolve(
                "custom",
                &[
                    VirtualBinding {
                        name: "Alt",
                        mask: 8,
                    },
                    VirtualBinding {
                        name: "Meta",
                        mask: 8,
                    },
                ],
            )
            .unwrap();
        assert_eq!(typ.select(8), expected, "{entries}");
    }
}

#[test]
fn keywords_and_named_levels_are_case_insensitive_but_names_are_not() {
    let source = "XKB_KEYMAP { XKB_KEYCODES {}; XKB_TYPES { VIRTUAL_MODIFIERS NumLock; TYPE \"Mixed\" { MODIFIERS=sHiFt+NumLock; MAP[shift]=lEvEl02; PRESERVE[Shift]=SHIFT; LEVEL_NAME[LEVEL2]=\"Shift\"; }; }; XKB_COMPATIBILITY {}; XKB_SYMBOLS {}; };";
    let typ = TypeCatalog::parse(source)
        .unwrap()
        .resolve("Mixed", &[])
        .unwrap();
    assert_eq!(
        typ.select(1),
        Selection {
            level: 1,
            consumed: 0
        }
    );
    assert!(TypeCatalog::parse(source)
        .unwrap()
        .resolve("mixed", &[])
        .is_err());
    assert!(TypeCatalog::parse(&source.replace("+NumLock", "+numlock")).is_err());
    for level in ["Level0x2", "LEVEL0X2", "Level", "Level17"] {
        assert!(
            TypeCatalog::parse(&map(&format!("type \"custom\" {{ map[none]={level}; }};")))
                .is_err(),
            "{level}"
        );
    }
    assert!(TypeCatalog::parse(&map("virtual_modifiers 1Foo;")).is_err());
}

#[test]
fn unused_unsupported_type_is_not_a_type_name_whitelist() {
    let catalog = TypeCatalog::parse(&map("type \"unused\" { exotic=Something(1); }; type \"usable\" { modifiers=Shift; map[Shift]=2; }; ")).unwrap();
    assert_eq!(catalog.resolve("usable", &[]).unwrap().select(1).level, 1);
    let error = catalog.resolve("unused", &[]).unwrap_err();
    assert_eq!(error.item, "unused.exotic");
    assert_eq!(error.reason, "unsupported type field");
    assert_eq!(catalog.resolve("absent", &[]).unwrap_err().item, "absent");
}

#[test]
fn duplicate_and_out_of_mask_definitions_refuse_ambiguity() {
    for bad in [
        "modifiers=Shift; modifiers=Lock;",
        "modifiers=Shift; map[Shift]=2; map[Shift]=3;",
        "modifiers=Shift; preserve[Shift]=Shift; preserve[Shift]=none;",
        "modifiers=Shift; map[Lock]=2;",
        "modifiers=Shift; preserve[Shift]=Lock;",
        "modifiers=Undefined;",
        "modifiers=0x100;",
        "modifiers=Shift+;",
        "map[Shift+Lock]=2;",
        "level_name[1]=\"a\"; level_name[1]=\"b\";",
        "modifiers=Shift map[Shift]=2;",
    ] {
        assert!(
            TypeCatalog::parse(&map(&format!("type \"bad\" {{ {bad} }};"))).is_err(),
            "{bad}"
        );
    }
    assert!(TypeCatalog::parse(&map("type \"x\" {}; type \"x\" {}; ")).is_err());
    assert!(TypeCatalog::parse(&map("virtual_modifiers V=Mod1,V=Mod2;")).is_err());
}

#[test]
fn lexical_comments_strings_and_nul_have_explicit_boundaries() {
    let good = map("/* fake } include \"file\" */ type \"include\\\"{}\" { modifiers=none; level_name[1]=\"é\\t\\\\\"; }; // comment\n");
    let catalog = TypeCatalog::parse(&(good.clone() + "\0")).unwrap();
    assert!(catalog.resolve("include\"{}", &[]).is_ok());
    assert!(TypeCatalog::parse(&map("/* interior \0 NUL */")).is_err());
    assert!(TypeCatalog::parse(
        &map("").replace("xkb_symbols {}", "xkb_symbols { INCLUDE \"us\"; } ")
    )
    .is_err());
    for bad in [
        good.clone() + "\0\0",
        good.clone() + "include \"x\"",
        good.clone() + "/*",
        good.replace("/* fake } include \"file\" */", "\0"),
        map("type \"\\q\" {};"),
        map("type \"x\" { modifiers=(Shift]; };"),
        map("include \"complete\""),
        good + "garbage",
    ] {
        assert!(TypeCatalog::parse(&bad).is_err(), "{bad:?}");
    }
}

#[test]
fn envelope_requires_one_self_contained_map_and_unique_components() {
    let good = map("type \"x\" {}; ");
    for bad in [
        String::new(),
        good.clone() + &good,
        good.replace("xkb_symbols {}", "xkb_types {}"),
        good.replace("xkb_symbols {};", ""),
        good.replace("xkb_symbols", "alien"),
        good.replace("xkb_keymap", "partial xkb_keymap"),
        good.trim_end_matches(';').to_owned(),
    ] {
        assert!(TypeCatalog::parse(&bad).is_err(), "{bad}");
    }
}

#[test]
fn limits_are_enforced_at_and_beyond_the_boundary() {
    let sixteen = table("modifiers=Shift; map[Shift]=16;")
        .resolve("custom", &[])
        .unwrap();
    assert_eq!(sixteen.levels(), 16);
    assert_eq!(sixteen.select(1).level, 15);
    for level in ["0", "17", "4294967296", "0x100000000"] {
        assert!(
            TypeCatalog::parse(&map(&format!("type \"x\" {{ map[none]={level}; }};"))).is_err()
        );
    }
    let types = (0..256)
        .map(|n| format!("type \"t{n}\" {{}}; "))
        .collect::<String>();
    assert_eq!(
        TypeCatalog::parse(&map(&types)).unwrap().names().count(),
        256
    );
    assert!(TypeCatalog::parse(&map(&(types + "type \"overflow\" {};")))
        .unwrap_err()
        .reason
        .contains("256"));
    let virtuals = (0..24)
        .map(|n| format!("V{n}"))
        .collect::<Vec<_>>()
        .join(",");
    assert!(TypeCatalog::parse(&map(&format!("virtual_modifiers {virtuals};"))).is_ok());
    assert!(TypeCatalog::parse(&map(&format!("virtual_modifiers {virtuals},Overflow;"))).is_err());
    let nested = |depth| {
        map(&format!(
            "type \"x\" {{ exotic={}0{}; }};",
            "(".repeat(depth),
            ")".repeat(depth)
        ))
    };
    assert!(TypeCatalog::parse(&nested(29)).is_ok());
    assert!(TypeCatalog::parse(&nested(30))
        .unwrap_err()
        .reason
        .contains("32"));
    let mut bytes = map("");
    bytes.extend(std::iter::repeat_n(' ', 1024 * 1024 - bytes.len()));
    assert!(TypeCatalog::parse(&bytes).is_ok());
    bytes.push(' ');
    assert!(TypeCatalog::parse(&bytes)
        .unwrap_err()
        .reason
        .contains("1 MiB"));
    assert!(TypeCatalog::parse(&";".repeat(200_001))
        .unwrap_err()
        .reason
        .contains("200000"));
    // Twenty envelope tokens plus foo= (2), 99989 terms (199977), ; (1).
    let exact_tokens = map("").replace(
        "xkb_symbols {}",
        &format!("xkb_symbols {{ foo={}1; }}", "1+".repeat(99_988)),
    );
    assert!(TypeCatalog::parse(&exact_tokens).is_ok());
    assert!(TypeCatalog::parse(&(exact_tokens + ";"))
        .unwrap_err()
        .reason
        .contains("200000"));
}

#[test]
fn generated_type_tables_match_a_direct_reference() {
    for seed in 0..64u32 {
        let mask = seed;
        let mut body = format!("modifiers={mask};");
        let mut reference = [(0usize, mask); 64];
        for state in 0..64u32 {
            if state & !mask != 0 || (state + seed).is_multiple_of(3) {
                continue;
            }
            let level = ((state * 7 + seed) % 16) as usize;
            let preserve = state & seed.rotate_left(1);
            body += &format!("map[{state}]={}; preserve[{state}]={preserve};", level + 1);
            reference[state as usize] = (level, mask & !preserve);
        }
        let typ = table(&body).resolve("custom", &[]).unwrap();
        for state in 0..256u32 {
            let (level, consumed) = reference[(state & mask) as usize];
            assert_eq!(
                typ.select(state),
                Selection { level, consumed },
                "seed={seed} state={state}"
            );
        }
    }
}

#[test]
fn arbitrary_truncations_and_byte_mutations_return_without_panics() {
    let source =
        map("virtual_modifiers NumLock; type \"K\" { modifiers=Shift+NumLock; map[NumLock]=2; }; ");
    for end in 0..source.len() {
        let _ = TypeCatalog::parse(&source[..end]);
    }
    for offset in 0..source.len() {
        for byte in [
            0, b'"', b'\\', b'{', b'}', b'[', b']', b';', b'+', b'/', b'9', 127,
        ] {
            let mut bytes = source.as_bytes().to_vec();
            bytes[offset] = byte;
            let source = String::from_utf8(bytes).unwrap();
            if let Ok(catalog) = TypeCatalog::parse(&source) {
                for name in catalog.names() {
                    let _ = catalog.resolve(name, &[]);
                }
            }
        }
    }
}
