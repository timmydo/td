//! ASCII and key-command keysyms. Unknown names remain diagnostic identities,
//! never physical-key fallback.

use crate::xkb::{Diagnostic, Result};
use crate::xkb_syntax::{Kind, Token};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Symbol {
    pub name: String,
    pub value: Option<u32>,
}

impl Symbol {
    pub fn parse(tokens: &[Token<'_>]) -> Result<Self> {
        let token = match tokens {
            [token] if token.kind == Kind::Word => token,
            _ => {
                return Err(crate::xkb_syntax::error(
                    "expected a single keysym per level",
                ))
            }
        };
        Ok(Self {
            name: token.text.to_owned(),
            value: value(token.text),
        })
    }
    pub fn matches(&self, other: &Self) -> bool {
        match (self.value, other.value) {
            (Some(a), Some(b)) => a == b,
            _ => self.name == other.name,
        }
    }
    pub fn error(&self, key: &str, reason: &'static str) -> Diagnostic {
        Diagnostic {
            offset: 0,
            item: format!("<{key}>:{}", self.name),
            reason,
        }
    }
    pub fn supported(&self) -> bool {
        self.value
            .is_some_and(|v| ascii(v).is_some() || command(v).is_some() || role(v).is_some())
    }
    pub fn ignored(&self) -> bool {
        self.name.starts_with("XF86")
            || self.value.is_some_and(|v| {
                matches!(v,
            0 | 0xffffff | 0xff13..=0xff15 | 0xff60..=0xff62 | 0xff67 | 0xff69..=0xff6b |
            0xff7e | 0xff9d | 0xffca..=0xffe0 | 0xffe6 | 0xffeb..=0xffee | 0xfe03..=0xfe05 | 0xfe11..=0xfe13 | 0x10080000..=0x1008ffff)
            })
    }
}

pub(crate) fn value(name: &str) -> Option<u32> {
    if name.len() == 1 {
        return name
            .as_bytes()
            .first()
            .filter(|b| (b' '..=b'~').contains(b))
            .map(|b| u32::from(*b));
    }
    if name.starts_with("0x") || name.starts_with("0X") {
        return crate::xkb::number(name);
    }
    if let Some(hex) = name
        .strip_prefix('U')
        .filter(|hex| hex.len() >= 4 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        let scalar = u32::from_str_radix(hex, 16).ok()?;
        char::from_u32(scalar)?;
        return Some(if scalar <= 255 {
            scalar
        } else {
            0x01000000 | scalar
        });
    }
    if let Some(n) = name
        .strip_prefix('F')
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|n| (1..=35).contains(n))
    {
        return Some(0xffbd + n);
    }
    if let Some(n) = name
        .strip_prefix("KP_")
        .filter(|s| s.len() == 1)
        .and_then(|s| s.parse::<u32>().ok())
    {
        return Some(0xffb0 + n);
    }
    NAMES
        .iter()
        .find(|(symbol, _)| *symbol == name)
        .map(|(_, value)| *value)
}

pub(crate) fn ascii(value: u32) -> Option<char> {
    let value = match value {
        0xff80 => 32,
        0xffaa => 42,
        0xffab => 43,
        0xffac => 44,
        0xffad => 45,
        0xffae => 46,
        0xffaf => 47,
        0xffbd => 61,
        0xffb0..=0xffb9 => value - 0xffb0 + 48,
        0x01000020..=0x0100007e => value - 0x01000000,
        _ => value,
    };
    (32..=126)
        .contains(&value)
        .then(|| char::from_u32(value))
        .flatten()
}

pub(crate) fn command(value: u32) -> Option<&'static str> {
    Some(match value {
        0xff08 => "Backspace",
        0xff09 | 0xfe20 | 0xff89 => "Tab",
        0xff0d | 0xff8d => "Return",
        0xff1b => "Escape",
        0xff50 | 0xff95 => "Home",
        0xff51 | 0xff96 => "Left",
        0xff52 | 0xff97 => "Up",
        0xff53 | 0xff98 => "Right",
        0xff54 | 0xff99 => "Down",
        0xff55 | 0xff9a => "PageUp",
        0xff56 | 0xff9b => "PageDown",
        0xff57 | 0xff9c => "End",
        0xff63 | 0xff9e => "Insert",
        0xffff | 0xff9f => "Delete",
        0xffbe => "F1",
        0xffbf => "F2",
        0xffc0 => "F3",
        0xffc1 => "F4",
        0xffc2 => "F5",
        0xffc3 => "F6",
        0xffc4 => "F7",
        0xffc5 => "F8",
        0xffc6 => "F9",
        0xffc7 => "F10",
        0xffc8 => "F11",
        0xffc9 => "F12",
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum Role {
    Shift,
    Caps,
    Control,
    Alt,
    Num,
}
pub(crate) fn role(value: u32) -> Option<Role> {
    match value {
        0xffe1 | 0xffe2 => Some(Role::Shift),
        0xffe3 | 0xffe4 => Some(Role::Control),
        0xffe5 => Some(Role::Caps),
        0xffe7..=0xffea => Some(Role::Alt),
        0xff7f => Some(Role::Num),
        _ => None,
    }
}

const NAMES: &[(&str, u32)] = &[
    ("NoSymbol", 0),
    ("VoidSymbol", 0xffffff),
    ("space", 32),
    ("exclam", 33),
    ("quotedbl", 34),
    ("numbersign", 35),
    ("dollar", 36),
    ("percent", 37),
    ("ampersand", 38),
    ("apostrophe", 39),
    ("quoteright", 39),
    ("parenleft", 40),
    ("parenright", 41),
    ("asterisk", 42),
    ("plus", 43),
    ("comma", 44),
    ("minus", 45),
    ("period", 46),
    ("slash", 47),
    ("colon", 58),
    ("semicolon", 59),
    ("less", 60),
    ("equal", 61),
    ("greater", 62),
    ("question", 63),
    ("at", 64),
    ("bracketleft", 91),
    ("backslash", 92),
    ("bracketright", 93),
    ("asciicircum", 94),
    ("underscore", 95),
    ("grave", 96),
    ("quoteleft", 96),
    ("braceleft", 123),
    ("bar", 124),
    ("braceright", 125),
    ("asciitilde", 126),
    ("BackSpace", 0xff08),
    ("Tab", 0xff09),
    ("Linefeed", 0xff0a),
    ("Return", 0xff0d),
    ("Pause", 0xff13),
    ("Scroll_Lock", 0xff14),
    ("Sys_Req", 0xff15),
    ("Escape", 0xff1b),
    ("Home", 0xff50),
    ("Left", 0xff51),
    ("Up", 0xff52),
    ("Right", 0xff53),
    ("Down", 0xff54),
    ("Prior", 0xff55),
    ("Page_Up", 0xff55),
    ("Next", 0xff56),
    ("Page_Down", 0xff56),
    ("End", 0xff57),
    ("Begin", 0xff58),
    ("Select", 0xff60),
    ("Print", 0xff61),
    ("Execute", 0xff62),
    ("Insert", 0xff63),
    ("Menu", 0xff67),
    ("Cancel", 0xff69),
    ("Help", 0xff6a),
    ("Break", 0xff6b),
    ("Mode_switch", 0xff7e),
    ("Num_Lock", 0xff7f),
    ("Delete", 0xffff),
    ("KP_Space", 0xff80),
    ("KP_Tab", 0xff89),
    ("KP_Enter", 0xff8d),
    ("KP_Home", 0xff95),
    ("KP_Left", 0xff96),
    ("KP_Up", 0xff97),
    ("KP_Right", 0xff98),
    ("KP_Down", 0xff99),
    ("KP_Prior", 0xff9a),
    ("KP_Page_Up", 0xff9a),
    ("KP_Next", 0xff9b),
    ("KP_Page_Down", 0xff9b),
    ("KP_End", 0xff9c),
    ("KP_Begin", 0xff9d),
    ("KP_Insert", 0xff9e),
    ("KP_Delete", 0xff9f),
    ("KP_Multiply", 0xffaa),
    ("KP_Add", 0xffab),
    ("KP_Separator", 0xffac),
    ("KP_Subtract", 0xffad),
    ("KP_Decimal", 0xffae),
    ("KP_Divide", 0xffaf),
    ("KP_Equal", 0xffbd),
    ("Shift_L", 0xffe1),
    ("Shift_R", 0xffe2),
    ("Control_L", 0xffe3),
    ("Control_R", 0xffe4),
    ("Caps_Lock", 0xffe5),
    ("Shift_Lock", 0xffe6),
    ("Meta_L", 0xffe7),
    ("Meta_R", 0xffe8),
    ("Alt_L", 0xffe9),
    ("Alt_R", 0xffea),
    ("Super_L", 0xffeb),
    ("Super_R", 0xffec),
    ("Hyper_L", 0xffed),
    ("Hyper_R", 0xffee),
    ("ISO_Left_Tab", 0xfe20),
    ("ISO_Level3_Shift", 0xfe03),
    ("ISO_Level3_Latch", 0xfe04),
    ("ISO_Level3_Lock", 0xfe05),
    ("ISO_Level5_Shift", 0xfe11),
    ("ISO_Level5_Latch", 0xfe12),
    ("ISO_Level5_Lock", 0xfe13),
];
