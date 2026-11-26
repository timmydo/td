//! The unnumbered CTAP HID report profile shared with td-jail.

pub const MAX_DESCRIPTOR: usize = 4096;

/// Firefox's reserved workload GID.
pub const FIREFOX_KEY_GROUP: u32 = 65_536;

pub fn key_mode(gid: u32, mode: u32) -> bool {
    matches!(
        (gid, mode & 0o7777),
        (0, 0o600) | (FIREFOX_KEY_GROUP, 0o600 | 0o660)
    )
}

/// Admit the unnumbered 64-byte FIDO application collection only.
pub fn descriptor(mut bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.is_empty() || bytes.len() > MAX_DESCRIPTOR {
        return Err("invalid HID descriptor size");
    }
    let (mut page, mut usage, mut size, mut count, mut min, mut max) =
        (None, None, None, None, None, None);
    let (mut opened, mut closed, mut input, mut output) = (false, false, false, false);
    while let Some(&prefix) = bytes.first() {
        if closed || prefix == 0xfe {
            return Err("unsupported HID descriptor item");
        }
        let len = match prefix & 3 {
            3 => 4,
            n => usize::from(n),
        };
        let raw = bytes.get(1..1 + len).ok_or("truncated HID descriptor")?;
        let mut value = 0u32;
        for (i, byte) in raw.iter().enumerate() {
            value |= u32::from(*byte) << (i * 8);
        }
        bytes = bytes.get(1 + len..).ok_or("truncated HID descriptor")?;
        match (prefix >> 2 & 3, prefix >> 4) {
            (1, 0) => page = Some(value),
            (1, 1) => min = Some(value),
            (1, 2) => max = Some(value),
            (1, 7) => size = Some(value),
            (1, 9) => count = Some(value),
            (2, 0) if usage.is_none() => usage = Some(value),
            (0, 10) if !opened && value == 1 && page == Some(0xf1d0) && usage == Some(1) => {
                opened = true;
                usage = None;
            }
            (0, kind @ (8 | 9))
                if opened
                    && value == 2
                    && page == Some(0xf1d0)
                    && size == Some(8)
                    && count == Some(64)
                    && min == Some(0)
                    && max == Some(255) =>
            {
                if kind == 8 && usage == Some(0x20) && !input {
                    input = true;
                } else if kind == 9 && usage == Some(0x21) && !output {
                    output = true;
                } else {
                    return Err("invalid FIDO report usage");
                }
                usage = None;
            }
            (0, 12) if opened && input && output && len == 0 && usage.is_none() => closed = true,
            _ => return Err("unsupported FIDO HID report profile"),
        }
    }
    if !closed {
        return Err("incomplete FIDO HID collection");
    }
    Ok(())
}

pub fn device_numbers(value: u64) -> (u64, u64) {
    (
        ((value >> 8) & 0xfff) | ((value >> 32) & 0xfffff000),
        (value & 0xff) | ((value >> 12) & 0xffffff00),
    )
}
