//! Owners of notebook plaintext and PINs on their way between the vault's
//! thread and the window: each zeroes its bytes when dropped and never
//! shows them through `Debug`. Compiler and allocator copies are outside
//! what they can reach.

use std::hint::black_box;

/// Text: an entry title or body, or a find query.
#[derive(Default, PartialEq, Eq)]
pub struct Text(String);

impl Text {
    pub fn new(text: String) -> Self {
        Self(text)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The text for an owner that clears it in turn; nothing is left here.
    pub fn take(mut self) -> String {
        std::mem::take(&mut self.0)
    }
}

impl Clone for Text {
    fn clone(&self) -> Self {
        Self(self.0.as_str().to_owned())
    }
}

impl Drop for Text {
    fn drop(&mut self) {
        wipe(std::mem::take(&mut self.0).into_bytes());
    }
}

/// Zeroes a buffer of plaintext, its spare capacity included, before it
/// is freed.
pub fn wipe(mut bytes: Vec<u8>) {
    bytes.fill(0);
    for byte in bytes.spare_capacity_mut() {
        byte.write(0);
    }
    black_box(bytes.as_mut_slice());
    black_box(bytes.spare_capacity_mut());
}

impl std::fmt::Debug for Text {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Text({} bytes)", self.0.len())
    }
}

/// Bytes: an entry body as stored, or a PIN, allocated at its exact size
/// so handing the box on moves it without a reallocation.
#[derive(Default)]
pub struct Bytes(Box<[u8]>);

impl Bytes {
    pub fn copy(bytes: &[u8]) -> Self {
        Self(Box::from(bytes))
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// The box for an owner that clears it in turn.
    pub fn take(mut self) -> Box<[u8]> {
        std::mem::take(&mut self.0)
    }
}

impl Drop for Bytes {
    fn drop(&mut self) {
        self.0.fill(0);
        black_box(&mut *self.0);
    }
}

impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bytes({} bytes)", self.0.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_content_and_take_leaves_nothing() {
        let text = Text::new("hunter2".to_owned());
        assert_eq!(format!("{text:?}"), "Text(7 bytes)");
        assert_eq!(text.clone().take(), "hunter2");
        let bytes = Bytes::copy(b"1234");
        assert_eq!(format!("{bytes:?}"), "Bytes(4 bytes)");
        assert_eq!(&*bytes.take(), b"1234");
    }
}
