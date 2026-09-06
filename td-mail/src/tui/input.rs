use std::io::{self, Read};

#[derive(Debug, Clone, PartialEq)]
pub enum Key {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Delete,
    Ctrl(char),
    AltEnter,
    MouseClick { row: u16, col: u16 },
    ScrollUp,
    ScrollDown,
}

/// Read a single keypress from stdin.
/// Returns None if no key is available (timeout expired).
pub fn read_key() -> Option<Key> {
    let mut buf = [0u8; 1];
    match io::stdin().read(&mut buf) {
        Ok(0) => None, // timeout, no data
        Ok(_) => Some(parse_byte(buf[0])),
        Err(_) => None,
    }
}

fn parse_byte(b: u8) -> Key {
    match b {
        13 => Key::Enter,
        27 => parse_escape(),
        127 => Key::Backspace,
        9 => Key::Tab,
        b @ 1..=26 => Key::Ctrl((b'a' + b - 1) as char),
        b if (32..127).contains(&b) => Key::Char(b as char),
        _ => Key::Char('?'),
    }
}

fn parse_escape() -> Key {
    // Try to read the next byte quickly
    let mut buf = [0u8; 1];
    match io::stdin().read(&mut buf) {
        Ok(0) => return Key::Escape, // bare escape
        Ok(_) => {}
        Err(_) => return Key::Escape,
    }

    if buf[0] == 13 {
        return Key::AltEnter; // ESC followed by Enter
    }

    if buf[0] != b'[' {
        return Key::Escape; // not a CSI sequence
    }

    // Read the sequence character
    match io::stdin().read(&mut buf) {
        Ok(0) => return Key::Escape,
        Ok(_) => {}
        Err(_) => return Key::Escape,
    }

    match buf[0] {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        // Extended sequences like ESC [ 5 ~ or CSI u like ESC [ 13 ; 7 u
        b'0'..=b'9' => parse_csi_number(buf[0]),
        // SGR mouse: ESC [ < ...
        b'<' => parse_sgr_mouse(),
        _ => Key::Escape,
    }
}

fn parse_csi_number(first_digit: u8) -> Key {
    let mut num: u16 = (first_digit - b'0') as u16;
    let mut buf = [0u8; 1];

    // Read remaining digits or terminator
    loop {
        match io::stdin().read(&mut buf) {
            Ok(0) | Err(_) => return Key::Escape,
            Ok(_) => {}
        }
        match buf[0] {
            b'0'..=b'9' => {
                num = num
                    .saturating_mul(10)
                    .saturating_add((buf[0] - b'0') as u16);
            }
            b'~' => {
                return match num {
                    3 => Key::Delete,
                    5 => Key::PageUp,
                    6 => Key::PageDown,
                    _ => Key::Escape,
                };
            }
            b';' => {
                // CSI u format: ESC [ keycode ; modifiers u
                // Read modifier number
                let mut modifiers: u16 = 0;
                loop {
                    match io::stdin().read(&mut buf) {
                        Ok(0) | Err(_) => return Key::Escape,
                        Ok(_) => {}
                    }
                    match buf[0] {
                        b'0'..=b'9' => {
                            modifiers = modifiers
                                .saturating_mul(10)
                                .saturating_add((buf[0] - b'0') as u16);
                        }
                        b'u' => {
                            return Key::Escape;
                        }
                        b'~' => return Key::Escape,
                        _ => return Key::Escape,
                    }
                }
            }
            _ => return Key::Escape,
        }
    }
}

fn parse_sgr_mouse() -> Key {
    // SGR format: ESC [ < btn ; col ; row M (press) or m (release)
    let mut params = [0u16; 3];
    let mut param_idx = 0;
    let mut buf = [0u8; 1];

    loop {
        match io::stdin().read(&mut buf) {
            Ok(0) | Err(_) => return Key::Escape,
            Ok(_) => {}
        }
        match buf[0] {
            b'0'..=b'9' => {
                // `get_mut` is the bound check: a fourth parameter has nowhere
                // to go and is dropped, as it was under `param_idx < 3`.
                if let Some(slot) = params.get_mut(param_idx) {
                    *slot = slot
                        .saturating_mul(10)
                        .saturating_add(u16::from(buf[0] - b'0'));
                }
            }
            b';' => {
                param_idx += 1;
                if param_idx >= 3 {
                    // Too many params, consume until terminator
                    loop {
                        match io::stdin().read(&mut buf) {
                            Ok(0) | Err(_) => return Key::Escape,
                            Ok(_) if buf[0] == b'M' || buf[0] == b'm' => return Key::Escape,
                            _ => {}
                        }
                    }
                }
            }
            b'M' => {
                if param_idx != 2 {
                    return Key::Escape;
                }
                return match params[0] {
                    0 => Key::MouseClick {
                        row: params[2],
                        col: params[1],
                    },
                    64 => Key::ScrollUp,
                    65 => Key::ScrollDown,
                    _ => Key::Escape,
                };
            }
            b'm' => {
                // Release event — ignore
                return Key::Escape;
            }
            _ => return Key::Escape,
        }
    }
}
