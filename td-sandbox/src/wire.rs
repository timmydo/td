use std::io::{self, Read, Write};

pub const INPUT: u8 = 1;
pub const SIZE: u8 = 2;
pub const EOF: u8 = 3;
pub const OUTPUT: u8 = 4;
pub const ERROR: u8 = 5;
pub const EXIT: u8 = 6;
pub const LIMIT: usize = 8192;

pub fn send(to: &mut impl Write, kind: u8, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > LIMIT {
        return Err(io::Error::other("oversized relay frame"));
    }
    to.write_all(&[kind])?;
    to.write_all(&(bytes.len() as u32).to_le_bytes())?;
    to.write_all(bytes)?;
    to.flush()
}

pub fn read(from: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0; 5];
    from.read_exact(&mut header)?;
    let [kind, a, b, c, d] = header;
    let len = u32::from_le_bytes([a, b, c, d]) as usize;
    if len > LIMIT || !(INPUT..=EXIT).contains(&kind) {
        return Err(io::Error::other("invalid relay frame"));
    }
    let mut bytes = vec![0; len];
    from.read_exact(&mut bytes)?;
    Ok((kind, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_large_and_truncated_frames() {
        let mut bytes = Vec::new();
        assert!(send(&mut bytes, INPUT, &vec![0; LIMIT + 1]).is_err());
        assert!(read(&mut &[INPUT, 255, 255, 255, 255][..]).is_err());
        assert!(read(&mut &[INPUT, 1, 0, 0, 0][..]).is_err());
        assert!(send(&mut bytes, OUTPUT, b"hello").is_ok());
        assert_eq!(
            read(&mut bytes.as_slice()).ok(),
            Some((OUTPUT, b"hello".to_vec()))
        );
    }
}
