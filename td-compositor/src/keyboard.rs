use crate::scene::SurfaceKey;
use std::collections::BTreeSet;

pub const XKB_KEYMAP: &str = r#"xkb_keymap {
xkb_keycodes "td" {
    minimum = 8;
    maximum = 255;
    <ESC> = 9;
    <AE01> = 10; <AE02> = 11; <AE03> = 12; <AE04> = 13;
    <AE05> = 14; <AE06> = 15; <AE07> = 16; <AE08> = 17;
    <AE09> = 18; <AE10> = 19; <AE11> = 20; <AE12> = 21;
    <BKSP> = 22; <TAB> = 23;
    <AD01> = 24; <AD02> = 25; <AD03> = 26; <AD04> = 27;
    <AD05> = 28; <AD06> = 29; <AD07> = 30; <AD08> = 31;
    <AD09> = 32; <AD10> = 33; <AD11> = 34; <AD12> = 35;
    <RTRN> = 36; <LCTL> = 37;
    <AC01> = 38; <AC02> = 39; <AC03> = 40; <AC04> = 41;
    <AC05> = 42; <AC06> = 43; <AC07> = 44; <AC08> = 45;
    <AC09> = 46; <AC10> = 47; <AC11> = 48; <TLDE> = 49;
    <LFSH> = 50; <BKSL> = 51;
    <AB01> = 52; <AB02> = 53; <AB03> = 54; <AB04> = 55;
    <AB05> = 56; <AB06> = 57; <AB07> = 58; <AB08> = 59;
    <AB09> = 60; <AB10> = 61;
    <RTSH> = 62; <KPMU> = 63; <LALT> = 64; <SPCE> = 65;
    <CAPS> = 66;
    <FK01> = 67; <FK02> = 68; <FK03> = 69; <FK04> = 70;
    <FK05> = 71; <FK06> = 72; <FK07> = 73; <FK08> = 74;
    <FK09> = 75; <FK10> = 76; <NMLK> = 77; <SCLK> = 78;
    <KP7> = 79; <KP8> = 80; <KP9> = 81; <KPSU> = 82;
    <KP4> = 83; <KP5> = 84; <KP6> = 85; <KPAD> = 86;
    <KP1> = 87; <KP2> = 88; <KP3> = 89;
    <KP0> = 90; <KPDL> = 91; <LSGT> = 94;
    <FK11> = 95; <FK12> = 96; <KPEN> = 104; <RCTL> = 105;
    <KPDV> = 106; <PRSC> = 107; <RALT> = 108;
    <HOME> = 110; <UP> = 111; <PGUP> = 112;
    <LEFT> = 113; <RGHT> = 114; <END> = 115;
    <DOWN> = 116; <PGDN> = 117; <INS> = 118; <DELE> = 119;
    <MUTE> = 121; <VOL-> = 122; <VOL+> = 123; <POWR> = 124;
    <KPEQ> = 125; <PAUS> = 127;
    <LWIN> = 133; <RWIN> = 134; <MENU> = 135;
};
xkb_types "td" {
    type "ONE_LEVEL" {
        modifiers = none;
        level_name[Level1] = "Base";
    };
    type "TWO_LEVEL" {
        modifiers = Shift;
        map[Shift] = Level2;
        level_name[Level1] = "Base";
        level_name[Level2] = "Shift";
    };
    type "ALPHABETIC" {
        modifiers = Shift+Lock;
        map[Shift] = Level2;
        map[Lock] = Level2;
        map[Shift+Lock] = Level1;
        level_name[Level1] = "Base";
        level_name[Level2] = "Caps";
    };
};
xkb_compatibility "td" {
    // Clients apply the compositor's modifier masks directly.
};
xkb_symbols "td" {
    name[group1] = "English (US)";
    key <ESC>  { [ Escape ] };
    key <AE01> { [ 1, exclam ] };
    key <AE02> { [ 2, at ] };
    key <AE03> { [ 3, numbersign ] };
    key <AE04> { [ 4, dollar ] };
    key <AE05> { [ 5, percent ] };
    key <AE06> { [ 6, asciicircum ] };
    key <AE07> { [ 7, ampersand ] };
    key <AE08> { [ 8, asterisk ] };
    key <AE09> { [ 9, parenleft ] };
    key <AE10> { [ 0, parenright ] };
    key <AE11> { [ minus, underscore ] };
    key <AE12> { [ equal, plus ] };
    key <BKSP> { [ BackSpace ] };
    key <TAB>  { [ Tab ] };
    key <AD01> { type="ALPHABETIC", [ q, Q ] };
    key <AD02> { type="ALPHABETIC", [ w, W ] };
    key <AD03> { type="ALPHABETIC", [ e, E ] };
    key <AD04> { type="ALPHABETIC", [ r, R ] };
    key <AD05> { type="ALPHABETIC", [ t, T ] };
    key <AD06> { type="ALPHABETIC", [ y, Y ] };
    key <AD07> { type="ALPHABETIC", [ u, U ] };
    key <AD08> { type="ALPHABETIC", [ i, I ] };
    key <AD09> { type="ALPHABETIC", [ o, O ] };
    key <AD10> { type="ALPHABETIC", [ p, P ] };
    key <AD11> { [ bracketleft, braceleft ] };
    key <AD12> { [ bracketright, braceright ] };
    key <RTRN> { [ Return ] };
    key <LCTL> { repeat=no, [ Control_L ] };
    key <AC01> { type="ALPHABETIC", [ a, A ] };
    key <AC02> { type="ALPHABETIC", [ s, S ] };
    key <AC03> { type="ALPHABETIC", [ d, D ] };
    key <AC04> { type="ALPHABETIC", [ f, F ] };
    key <AC05> { type="ALPHABETIC", [ g, G ] };
    key <AC06> { type="ALPHABETIC", [ h, H ] };
    key <AC07> { type="ALPHABETIC", [ j, J ] };
    key <AC08> { type="ALPHABETIC", [ k, K ] };
    key <AC09> { type="ALPHABETIC", [ l, L ] };
    key <AC10> { [ semicolon, colon ] };
    key <AC11> { [ apostrophe, quotedbl ] };
    key <TLDE> { [ grave, asciitilde ] };
    key <LFSH> { repeat=no, [ Shift_L ] };
    key <BKSL> { [ backslash, bar ] };
    key <AB01> { type="ALPHABETIC", [ z, Z ] };
    key <AB02> { type="ALPHABETIC", [ x, X ] };
    key <AB03> { type="ALPHABETIC", [ c, C ] };
    key <AB04> { type="ALPHABETIC", [ v, V ] };
    key <AB05> { type="ALPHABETIC", [ b, B ] };
    key <AB06> { type="ALPHABETIC", [ n, N ] };
    key <AB07> { type="ALPHABETIC", [ m, M ] };
    key <AB08> { [ comma, less ] };
    key <AB09> { [ period, greater ] };
    key <AB10> { [ slash, question ] };
    key <RTSH> { repeat=no, [ Shift_R ] };
    key <KPMU> { [ KP_Multiply ] };
    key <LALT> { repeat=no, [ Alt_L ] };
    key <SPCE> { [ space ] };
    key <CAPS> { repeat=no, [ Caps_Lock ] };
    key <FK01> { [ F1 ] }; key <FK02> { [ F2 ] };
    key <FK03> { [ F3 ] }; key <FK04> { [ F4 ] };
    key <FK05> { [ F5 ] }; key <FK06> { [ F6 ] };
    key <FK07> { [ F7 ] }; key <FK08> { [ F8 ] };
    key <FK09> { [ F9 ] }; key <FK10> { [ F10 ] };
    key <FK11> { [ F11 ] }; key <FK12> { [ F12 ] };
    key <NMLK> { repeat=no, [ Num_Lock ] };
    key <SCLK> { repeat=no, [ Scroll_Lock ] };
    key <KP7>  { [ KP_7 ] }; key <KP8> { [ KP_8 ] };
    key <KP9>  { [ KP_9 ] }; key <KPSU> { [ KP_Subtract ] };
    key <KP4>  { [ KP_4 ] }; key <KP5> { [ KP_5 ] };
    key <KP6>  { [ KP_6 ] }; key <KPAD> { [ KP_Add ] };
    key <KP1>  { [ KP_1 ] }; key <KP2> { [ KP_2 ] };
    key <KP3>  { [ KP_3 ] }; key <KP0> { [ KP_0 ] };
    key <KPDL> { [ KP_Decimal ] }; key <KPEN> { [ KP_Enter ] };
    key <KPDV> { [ KP_Divide ] }; key <KPEQ> { [ KP_Equal ] };
    key <LSGT> { [ less, greater ] };
    key <RCTL> { repeat=no, [ Control_R ] };
    key <PRSC> { [ Print ] };
    key <RALT> { repeat=no, [ Alt_R ] };
    key <HOME> { [ Home ] }; key <UP> { [ Up ] };
    key <PGUP> { [ Prior ] }; key <LEFT> { [ Left ] };
    key <RGHT> { [ Right ] }; key <END> { [ End ] };
    key <DOWN> { [ Down ] }; key <PGDN> { [ Next ] };
    key <INS> { [ Insert ] }; key <DELE> { [ Delete ] };
    key <MUTE> { [ XF86AudioMute ] };
    key <VOL-> { [ XF86AudioLowerVolume ] };
    key <VOL+> { [ XF86AudioRaiseVolume ] };
    key <POWR> { [ XF86PowerOff ] };
    key <PAUS> { [ Pause ] };
    key <LWIN> { repeat=no, [ Super_L ] };
    key <RWIN> { repeat=no, [ Super_R ] };
    key <MENU> { [ Menu ] };
    modifier_map Shift { <LFSH>, <RTSH> };
    modifier_map Lock { <CAPS> };
    modifier_map Control { <LCTL>, <RCTL> };
    modifier_map Mod1 { <LALT>, <RALT> };
    modifier_map Mod2 { <NMLK> };
    modifier_map Mod4 { <LWIN>, <RWIN> };
};
};
"#;

pub const MOD_SHIFT: u32 = 1 << 0;
pub const MOD_CAPS: u32 = 1 << 1;
pub const MOD_CONTROL: u32 = 1 << 2;
pub const MOD_ALT: u32 = 1 << 3;
pub const MOD_NUM: u32 = 1 << 4;
pub const MOD_LOGO: u32 = 1 << 6;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ModifierState {
    pub depressed: u32,
    pub latched: u32,
    pub locked: u32,
    pub group: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyState {
    Released,
    Pressed,
}

impl KeyState {
    pub fn wire(self) -> u32 {
        match self {
            KeyState::Released => 0,
            KeyState::Pressed => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyInput {
    pub time: u32,
    pub key: u32,
    pub state: KeyState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyboardEvent {
    Enter {
        surface: SurfaceKey,
        keys: Vec<u32>,
    },
    Leave {
        surface: SurfaceKey,
    },
    Key {
        surface: SurfaceKey,
        input: KeyInput,
    },
    Modifiers {
        surface: SurfaceKey,
        state: ModifierState,
    },
}

impl KeyboardEvent {
    pub fn surface(&self) -> SurfaceKey {
        match self {
            KeyboardEvent::Enter { surface, .. }
            | KeyboardEvent::Leave { surface }
            | KeyboardEvent::Key { surface, .. }
            | KeyboardEvent::Modifiers { surface, .. } => *surface,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoutedKeyboardEvent {
    pub revision: u64,
    pub event: KeyboardEvent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyboardSnapshot {
    pub revision: u64,
    pub focus: Option<SurfaceKey>,
    pub keys: Vec<u32>,
    pub modifiers: ModifierState,
}

#[derive(Default)]
pub struct KeyboardState {
    revision: u64,
    focus: Option<SurfaceKey>,
    pressed: BTreeSet<u32>,
    modifiers: ModifierState,
}

impl KeyboardState {
    pub fn snapshot(&self) -> KeyboardSnapshot {
        KeyboardSnapshot {
            revision: self.revision,
            focus: self.focus,
            keys: self.pressed.iter().copied().collect(),
            modifiers: self.modifiers,
        }
    }

    pub fn set_focus(
        &mut self,
        focus: Option<SurfaceKey>,
    ) -> Result<Vec<RoutedKeyboardEvent>, String> {
        if self.focus == focus {
            return Ok(Vec::new());
        }
        let revision = self.advance()?;
        let mut events = Vec::with_capacity(3);
        if let Some(surface) = self.focus {
            events.push(RoutedKeyboardEvent {
                revision,
                event: KeyboardEvent::Leave { surface },
            });
        }
        self.focus = focus;
        if let Some(surface) = focus {
            events.push(RoutedKeyboardEvent {
                revision,
                event: KeyboardEvent::Enter {
                    surface,
                    keys: self.pressed.iter().copied().collect(),
                },
            });
            events.push(RoutedKeyboardEvent {
                revision,
                event: KeyboardEvent::Modifiers {
                    surface,
                    state: self.modifiers,
                },
            });
        }
        Ok(events)
    }

    pub fn key(&mut self, input: KeyInput) -> Result<Option<RoutedKeyboardEvent>, String> {
        let changed = match input.state {
            KeyState::Pressed => !self.pressed.contains(&input.key),
            KeyState::Released => self.pressed.contains(&input.key),
        };
        if !changed {
            return Ok(None);
        }
        let revision = self.advance()?;
        match input.state {
            KeyState::Pressed => {
                self.pressed.insert(input.key);
            }
            KeyState::Released => {
                self.pressed.remove(&input.key);
            }
        }
        Ok(self.focus.map(|surface| RoutedKeyboardEvent {
            revision,
            event: KeyboardEvent::Key { surface, input },
        }))
    }

    pub fn modifiers(
        &mut self,
        modifiers: ModifierState,
    ) -> Result<Option<RoutedKeyboardEvent>, String> {
        if self.modifiers == modifiers {
            return Ok(None);
        }
        let revision = self.advance()?;
        self.modifiers = modifiers;
        Ok(self.focus.map(|surface| RoutedKeyboardEvent {
            revision,
            event: KeyboardEvent::Modifiers {
                surface,
                state: modifiers,
            },
        }))
    }

    fn advance(&mut self) -> Result<u64, String> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| "keyboard revision exhausted".to_string())?;
        Ok(self.revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(client: u64, object: u32) -> SurfaceKey {
        SurfaceKey { client, object }
    }

    fn press(time: u32, key: u32) -> KeyInput {
        KeyInput {
            time,
            key,
            state: KeyState::Pressed,
        }
    }

    #[test]
    fn focus_transitions_leave_then_enter_with_held_state() {
        let mut state = KeyboardState::default();
        assert!(state.key(press(10, 125)).unwrap().is_none());
        let logo = ModifierState {
            depressed: MOD_LOGO,
            ..ModifierState::default()
        };
        assert!(state.modifiers(logo).unwrap().is_none());
        assert_eq!(
            state.set_focus(Some(key(1, 7))).unwrap(),
            [
                RoutedKeyboardEvent {
                    revision: 3,
                    event: KeyboardEvent::Enter {
                        surface: key(1, 7),
                        keys: vec![125],
                    },
                },
                RoutedKeyboardEvent {
                    revision: 3,
                    event: KeyboardEvent::Modifiers {
                        surface: key(1, 7),
                        state: logo,
                    },
                },
            ]
        );
        assert_eq!(
            state.set_focus(Some(key(2, 9))).unwrap(),
            [
                RoutedKeyboardEvent {
                    revision: 4,
                    event: KeyboardEvent::Leave { surface: key(1, 7) },
                },
                RoutedKeyboardEvent {
                    revision: 4,
                    event: KeyboardEvent::Enter {
                        surface: key(2, 9),
                        keys: vec![125],
                    },
                },
                RoutedKeyboardEvent {
                    revision: 4,
                    event: KeyboardEvent::Modifiers {
                        surface: key(2, 9),
                        state: logo,
                    },
                },
            ]
        );
        assert!(state.set_focus(Some(key(2, 9))).unwrap().is_empty());
    }

    #[test]
    fn key_and_modifier_events_are_focused_and_snapshot_is_stable() {
        let mut state = KeyboardState::default();
        state.set_focus(Some(key(4, 2))).unwrap();
        let input = press(77, 30);
        assert_eq!(
            state.key(input).unwrap(),
            Some(RoutedKeyboardEvent {
                revision: 2,
                event: KeyboardEvent::Key {
                    surface: key(4, 2),
                    input,
                },
            })
        );
        let modifiers = ModifierState {
            depressed: MOD_SHIFT | MOD_CONTROL,
            locked: MOD_CAPS,
            ..ModifierState::default()
        };
        assert_eq!(
            state.modifiers(modifiers).unwrap(),
            Some(RoutedKeyboardEvent {
                revision: 3,
                event: KeyboardEvent::Modifiers {
                    surface: key(4, 2),
                    state: modifiers,
                },
            })
        );
        assert!(state.modifiers(modifiers).unwrap().is_none());
        assert_eq!(
            state.snapshot(),
            KeyboardSnapshot {
                revision: 3,
                focus: Some(key(4, 2)),
                keys: vec![30],
                modifiers,
            }
        );
    }

    #[test]
    fn duplicate_presses_and_unmatched_releases_do_not_change_logical_state() {
        let mut state = KeyboardState::default();
        state.set_focus(Some(key(4, 2))).unwrap();
        assert!(state.key(press(7, 30)).unwrap().is_some());
        assert!(state.key(press(8, 30)).unwrap().is_none());
        assert!(state
            .key(KeyInput {
                time: 9,
                key: 31,
                state: KeyState::Released,
            })
            .unwrap()
            .is_none());
        assert_eq!(state.snapshot().revision, 2);
        assert_eq!(state.snapshot().keys, [30]);
    }

    #[test]
    fn releases_clear_the_enter_key_set_even_without_focus() {
        let mut state = KeyboardState::default();
        state.key(press(1, 48)).unwrap();
        state
            .key(KeyInput {
                time: 2,
                key: 48,
                state: KeyState::Released,
            })
            .unwrap();
        assert!(state.snapshot().keys.is_empty());
        assert_eq!(state.snapshot().revision, 2);
        assert!(state.set_focus(None).unwrap().is_empty());
    }

    #[test]
    fn wire_states_and_event_surface_cover_every_variant() {
        assert_eq!(KeyState::Released.wire(), 0);
        assert_eq!(KeyState::Pressed.wire(), 1);
        let surface = key(1, 2);
        for event in [
            KeyboardEvent::Enter {
                surface,
                keys: Vec::new(),
            },
            KeyboardEvent::Leave { surface },
            KeyboardEvent::Key {
                surface,
                input: press(0, 1),
            },
            KeyboardEvent::Modifiers {
                surface,
                state: ModifierState::default(),
            },
        ] {
            assert_eq!(event.surface(), surface);
        }
    }

    #[test]
    fn bundled_us_keymap_is_self_contained_and_covers_protocol_modifiers() {
        assert!(XKB_KEYMAP.starts_with("xkb_keymap {"));
        assert!(XKB_KEYMAP.ends_with("};\n"));
        assert!(!XKB_KEYMAP.contains("include"));
        for needle in [
            "<AE01> = 10",
            "<AD01> = 24",
            "<AC01> = 38",
            "<AB01> = 52",
            "<LCTL> = 37",
            "<LFSH> = 50",
            "<LALT> = 64",
            "<RALT> = 108",
            "<LWIN> = 133",
            "modifier_map Lock",
            "modifier_map Control",
            "modifier_map Mod1",
            "modifier_map Mod2",
            "modifier_map Mod4",
        ] {
            assert!(XKB_KEYMAP.contains(needle), "{needle}");
        }
        for name in [
            "LCTL", "RCTL", "LFSH", "RTSH", "LALT", "RALT", "CAPS", "NMLK", "SCLK", "LWIN", "RWIN",
        ] {
            let prefix = format!("key <{name}>");
            let declaration = XKB_KEYMAP
                .lines()
                .filter(|line| line.trim_start().starts_with(&prefix))
                .map(str::trim)
                .next()
                .unwrap();
            assert!(declaration.contains("repeat=no"), "{declaration}");
        }
        for name in ["AE01", "AD01", "AC01", "AB01", "SPCE", "FK01"] {
            let prefix = format!("key <{name}>");
            let declaration = XKB_KEYMAP
                .lines()
                .filter(|line| line.trim_start().starts_with(&prefix))
                .map(str::trim)
                .next()
                .unwrap();
            assert!(!declaration.contains("repeat=no"), "{declaration}");
        }
    }

    #[test]
    fn bundled_us_keymap_has_consistent_structure_and_key_references() {
        let mut depth = 0i32;
        for character in XKB_KEYMAP.chars() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    assert!(depth >= 0);
                }
                _ => {}
            }
        }
        assert_eq!(depth, 0);
        assert_eq!(XKB_KEYMAP.matches('"').count() % 2, 0);

        let keycodes = XKB_KEYMAP
            .split("xkb_keycodes")
            .nth(1)
            .unwrap()
            .split("xkb_types")
            .next()
            .unwrap();
        let mut names = BTreeSet::new();
        let mut values = BTreeSet::new();
        for statement in keycodes.split(';') {
            let Some(name) = statement
                .split('<')
                .nth(1)
                .and_then(|tail| tail.split('>').next())
            else {
                continue;
            };
            let value: u16 = statement.split('=').nth(1).unwrap().trim().parse().unwrap();
            assert!(names.insert(name));
            assert!(values.insert(value));
            assert!((8..=255).contains(&value));
        }
        assert_eq!(names.len(), 110);

        let symbols = XKB_KEYMAP.split("xkb_symbols").nth(1).unwrap();
        let mut declared = BTreeSet::new();
        for statement in symbols.split(';') {
            let trimmed = statement.trim_start();
            if trimmed.starts_with("key <") {
                let name = trimmed
                    .split('<')
                    .nth(1)
                    .and_then(|tail| tail.split('>').next())
                    .unwrap();
                assert!(declared.insert(name));
                assert!(trimmed.contains('['));
                assert!(trimmed.contains(']'));
            }
            for name in statement
                .split('<')
                .skip(1)
                .filter_map(|tail| tail.split('>').next())
            {
                assert!(names.contains(name), "{name}");
            }
        }
        assert_eq!(declared, names);
        for name in ["ONE_LEVEL", "TWO_LEVEL", "ALPHABETIC"] {
            assert!(XKB_KEYMAP.contains(&format!("type \"{name}\"")));
        }
    }

    #[test]
    fn revision_exhaustion_fails_without_mutating_keyboard_state() {
        let mut state = KeyboardState {
            revision: u64::MAX,
            ..KeyboardState::default()
        };
        assert!(state.key(press(1, 30)).is_err());
        assert!(state
            .modifiers(ModifierState {
                depressed: MOD_SHIFT,
                ..ModifierState::default()
            })
            .is_err());
        assert!(state.set_focus(Some(key(1, 2))).is_err());
        assert_eq!(
            state.snapshot(),
            KeyboardSnapshot {
                revision: u64::MAX,
                focus: None,
                keys: Vec::new(),
                modifiers: ModifierState::default(),
            }
        );
    }
}
