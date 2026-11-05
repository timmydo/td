//! The whole of this crate's `unsafe` (UNSAFE.md §26): calling a
//! `#[target_feature]` function from code compiled without that feature, which
//! is sound only on a CPU that has it.
//!
//! A value of each type below is the proof. Only `detect` makes one and it asks
//! the CPU first, so a method taking `self` has already asked. The instructions
//! themselves sit in `aes_ni`, `chacha_avx2` and `sha_ni`, which need no `unsafe`:
//! there, intrinsics that touch no pointer are safe to call.

use crate::{aes_ni, chacha_avx2, sha_ni};

/// AES-NI and PCLMULQDQ, with the byte shuffles and inserts that work needs.
#[derive(Clone, Copy)]
pub(crate) struct AesClmul(());

impl AesClmul {
    pub(crate) fn detect() -> Option<Self> {
        (is_x86_feature_detected!("aes")
            && is_x86_feature_detected!("pclmulqdq")
            && is_x86_feature_detected!("ssse3")
            && is_x86_feature_detected!("sse4.1"))
        .then_some(Self(()))
    }

    #[allow(unsafe_code)]
    pub(crate) fn new_key(self, schedule: &[u32], wide: bool) -> Box<aes_ni::Key> {
        // SAFETY: `self` shows the CPU has aes_ni's features.
        unsafe { aes_ni::new_key(schedule, wide) }
    }

    #[allow(unsafe_code)]
    pub(crate) fn encrypt_block(self, key: &aes_ni::Key, block: &[u8; 16]) -> [u8; 16] {
        // SAFETY: `self` shows the CPU has aes_ni's features.
        unsafe { aes_ni::encrypt_block(key, block) }
    }

    #[allow(unsafe_code)]
    pub(crate) fn ctr(self, key: &aes_ni::Key, nonce: &[u8; 12], first: u32, data: &mut [u8]) {
        // SAFETY: `self` shows the CPU has aes_ni's features.
        unsafe { aes_ni::ctr(key, nonce, first, data) }
    }

    #[allow(unsafe_code)]
    pub(crate) fn ghash(self, key: &aes_ni::Key, y: &mut [u64; 2], data: &[u8]) {
        // SAFETY: `self` shows the CPU has aes_ni's features.
        unsafe { aes_ni::ghash(key, y, data) }
    }
}

/// AVX2, for eight ChaCha20 blocks at a time.
#[derive(Clone, Copy)]
pub(crate) struct Avx2(());

impl Avx2 {
    pub(crate) fn detect() -> Option<Self> {
        is_x86_feature_detected!("avx2").then_some(Self(()))
    }

    #[allow(unsafe_code)]
    pub(crate) fn chacha20(self, key: &[u32; 8], nonce: &[u32; 3], first: u32, data: &mut [u8]) {
        // SAFETY: `self` shows the CPU has chacha_avx2's feature.
        unsafe { chacha_avx2::crypt(key, nonce, first, data) }
    }
}

/// The SHA extensions, with the byte shuffles and inserts that work needs.
#[derive(Clone, Copy)]
pub(crate) struct ShaNi(());

impl ShaNi {
    pub(crate) fn detect() -> Option<Self> {
        (is_x86_feature_detected!("sha")
            && is_x86_feature_detected!("ssse3")
            && is_x86_feature_detected!("sse4.1"))
        .then_some(Self(()))
    }

    #[allow(unsafe_code)]
    pub(crate) fn compress256(self, h: &mut [u32; 8], block: &[u8; 64]) {
        // SAFETY: `self` shows the CPU has sha_ni's features.
        unsafe { sha_ni::compress256(h, block) }
    }
}
