//! Known answers for what td-ssh takes from this crate, from sources independent of it:
//! NIST's ACVP, OpenSSL, Python's hashlib and hmac, OpenBSD's regress vectors,
//! ssh-keygen's own files and the naive references of `ssh_vectors.py` (DESIGN.md).
//! Each corpus states its row count, so a truncated or filtered file reds rather than
//! passing on less.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use td_primitives::aes256_ctr::Aes256Ctr;
use td_primitives::bcrypt_pbkdf::bcrypt_pbkdf;
use td_primitives::chacha20_poly1305::OpenSshAead;
use td_primitives::curve25519::ed25519::{self, SigningKey};
use td_primitives::hmac::hmac_sha1;
use td_primitives::keccak::{sha3_256, sha3_512, Shake128, Shake256};
use td_primitives::mlkem768;
use td_primitives::Refused;

fn hex(text: &str) -> Vec<u8> {
    if text == "-" {
        return Vec::new();
    }
    assert!(text.len().is_multiple_of(2), "{text}");
    text.as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn array<const N: usize>(text: &str) -> [u8; N] {
    hex(text).try_into().unwrap()
}

fn rows(text: &'static str) -> impl Iterator<Item = Vec<&'static str>> {
    text.lines()
        .filter(|line| !line.starts_with('#'))
        .map(|line| line.split(' ').collect())
}

/// The generators' `message[i] = (i * 17 + length) & 0xff`.
fn message(length: usize) -> Vec<u8> {
    (0..length)
        .map(|i| ((i * 17 + length) & 0xff) as u8)
        .collect()
}

const SPLITS: &[usize] = &[
    1, 7, 55, 56, 63, 64, 65, 72, 111, 112, 127, 128, 129, 136, 137, 168,
];

#[test]
fn keccak_agrees_with_python_at_every_block_boundary() {
    let mut seen = [0usize; 4];
    for row in rows(include_str!("ssh/keccak.txt")) {
        let [algorithm, length, digest] = row[..] else {
            panic!("row")
        };
        let m = message(length.parse().unwrap());
        let digest = hex(digest);
        match algorithm {
            "sha3_256" => {
                assert_eq!(sha3_256(&m).as_slice(), digest);
                seen[0] += 1;
            }
            "sha3_512" => {
                assert_eq!(sha3_512(&m).as_slice(), digest);
                seen[1] += 1;
            }
            "shake128" => {
                let mut xof = Shake128::new();
                xof.absorb(&m);
                let mut out = vec![0u8; digest.len()];
                xof.squeeze(&mut out);
                assert_eq!(out, digest);
                // Split absorbs and many short squeezes give the same stream.
                for at in SPLITS.iter().filter(|at| **at <= m.len()) {
                    let mut xof = Shake128::new();
                    xof.absorb(&m[..*at]);
                    xof.absorb(&m[*at..]);
                    let mut out = vec![0u8; digest.len()];
                    for chunk in out.chunks_mut(*at) {
                        xof.squeeze(chunk);
                    }
                    assert_eq!(out, digest);
                }
                seen[2] += 1;
            }
            "shake256" => {
                let mut xof = Shake256::new();
                xof.absorb(&m);
                let mut out = vec![0u8; digest.len()];
                xof.squeeze(&mut out);
                assert_eq!(out, digest);
                for at in SPLITS.iter().filter(|at| **at <= m.len()) {
                    let mut xof = Shake256::new();
                    xof.absorb(&m[..*at]);
                    xof.absorb(&m[*at..]);
                    let mut out = vec![0u8; digest.len()];
                    for chunk in out.chunks_mut(*at) {
                        xof.squeeze(chunk);
                    }
                    assert_eq!(out, digest);
                }
                seen[3] += 1;
            }
            other => panic!("{other}"),
        }
    }
    assert_eq!(seen, [63, 63, 67, 67]);
}

#[test]
fn shake_absorb_after_squeeze_is_ignored_and_empty_squeeze_is_harmless() {
    let mut once = Shake128::new();
    once.absorb(b"abc");
    let mut expected = [0u8; 400];
    once.squeeze(&mut expected);

    let mut xof = Shake128::new();
    xof.absorb(b"abc");
    xof.squeeze(&mut []);
    let mut out = [0u8; 400];
    let (head, tail) = out.split_at_mut(168);
    xof.squeeze(head);
    xof.absorb(b"ignored");
    xof.squeeze(&mut []);
    xof.squeeze(tail);
    assert_eq!(out, expected);

    // An empty squeeze still pads, so a later absorb cannot change the stream.
    let mut xof = Shake256::new();
    xof.squeeze(&mut []);
    xof.absorb(b"ignored");
    let mut first = [0u8; 32];
    xof.squeeze(&mut first);
    let mut empty = Shake256::new();
    let mut second = [0u8; 32];
    empty.squeeze(&mut second);
    assert_eq!(first, second);
}

#[test]
fn hmac_sha1_agrees_with_python() {
    let mut count = 0;
    for row in rows(include_str!("ssh/hmac_sha1.txt")) {
        let [key_length, length, mac] = row[..] else {
            panic!("row")
        };
        let key_length: usize = key_length.parse().unwrap();
        let key: Vec<u8> = (0..key_length)
            .map(|i| ((i * 31 + key_length + 7) & 0xff) as u8)
            .collect();
        assert_eq!(
            hmac_sha1(&key, &message(length.parse().unwrap())).as_slice(),
            hex(mac)
        );
        count += 1;
    }
    assert_eq!(count, 110);
}

#[test]
fn the_openssh_aead_agrees_with_openssl_composition() {
    let mut count = 0;
    for row in rows(include_str!("ssh/chachapoly.txt")) {
        let [key, seq, length, sealed, tag] = row[..] else {
            panic!("row")
        };
        let aead = OpenSshAead::new(&array(key));
        let seq: u32 = seq.parse().unwrap();
        let length: usize = length.parse().unwrap();
        let sealed = hex(sealed);
        let tag: [u8; 16] = array(tag);
        let mut plain = (length as u32).to_be_bytes().to_vec();
        plain.extend_from_slice(&message(length));

        let mut packet = plain.clone();
        assert_eq!(aead.seal(seq, &mut packet), Ok(tag), "{length}");
        assert_eq!(packet, sealed, "{length}");
        let head: [u8; 4] = packet[..4].try_into().unwrap();
        assert_eq!(
            aead.decrypt_length(seq, &head),
            (length as u32).to_be_bytes()
        );

        let mut opened = sealed.clone();
        aead.open(seq, &mut opened, &tag).unwrap();
        // The caller decrypted the length; open leaves those bytes alone.
        assert_eq!(opened[..4], sealed[..4]);
        assert_eq!(opened[4..], plain[4..], "{length}");
        count += 1;
    }
    assert_eq!(count, 52);
}

#[test]
fn the_openssh_aead_refuses_tampering_and_leaves_the_packet_untouched() {
    let aead = OpenSshAead::new(&[0x42; 64]);
    for length in [0usize, 1, 64, 300] {
        let mut sealed = (length as u32).to_be_bytes().to_vec();
        sealed.extend_from_slice(&message(length));
        let tag = aead.seal(7, &mut sealed).unwrap();
        let refused = |packet: &[u8], seq: u32, tag: &[u8; 16]| {
            let mut copy = packet.to_vec();
            assert_eq!(aead.open(seq, &mut copy, tag), Err(Refused), "{length}");
            assert_eq!(copy, packet);
        };
        // Every bit of the length, of the tag, and spread over the payload.
        for bit in 0..32 {
            let mut changed = sealed.clone();
            changed[bit / 8] ^= 1 << (bit % 8);
            refused(&changed, 7, &tag);
        }
        for bit in 0..128 {
            let mut changed = tag;
            changed[bit / 8] ^= 1 << (bit % 8);
            refused(&sealed, 7, &changed);
        }
        for at in (4..sealed.len())
            .step_by(11)
            .chain(sealed.len().checked_sub(1))
        {
            if at >= 4 {
                let mut changed = sealed.clone();
                changed[at] ^= 0x80;
                refused(&changed, 7, &tag);
            }
        }
        refused(&sealed, 8, &tag);
        refused(&sealed[..sealed.len() - 1], 7, &tag);
        let mut extended = sealed.clone();
        extended.push(0);
        refused(&extended, 7, &tag);

        // K_2 alone sets the tag, and K_1 alone the length.
        let mut main_key = [0x42u8; 64];
        main_key[0] ^= 1;
        let mut copy = sealed.clone();
        assert!(OpenSshAead::new(&main_key)
            .open(7, &mut copy, &tag)
            .is_err());
        let mut length_key = [0x42u8; 64];
        length_key[32] ^= 1;
        let other = OpenSshAead::new(&length_key);
        let head: [u8; 4] = sealed[..4].try_into().unwrap();
        assert_ne!(
            other.decrypt_length(7, &head),
            aead.decrypt_length(7, &head)
        );
        let mut copy = sealed.clone();
        other.open(7, &mut copy, &tag).unwrap();
    }
}

// A zero tag here would look like success to a caller that sent it: this is refused.
#[test]
fn a_packet_too_short_for_a_length_is_refused_both_ways() {
    let aead = OpenSshAead::new(&[1; 64]);
    for length in 0..4usize {
        let mut packet = vec![0xa5u8; length];
        assert_eq!(aead.seal(0, &mut packet), Err(Refused));
        assert_eq!(packet, vec![0xa5u8; length]);
        assert_eq!(aead.open(0, &mut packet, &[0; 16]), Err(Refused));
        assert_eq!(packet, vec![0xa5u8; length]);
    }
    let mut empty_payload = [0u8; 4];
    assert!(aead.seal(0, &mut empty_payload).is_ok());
}

#[test]
fn ed25519_edges_follow_rfc_8032_and_the_eight_torsion_points_are_small_order() {
    let mut count = 0;
    let mut torsion = std::collections::BTreeSet::new();
    for row in rows(include_str!("ssh/ed25519_edges.txt")) {
        let [public, msg, sig, expected, _, note] = row[..] else {
            panic!("row")
        };
        let public: [u8; 32] = array(public);
        let accepted = ed25519::verify(&public, &hex(msg), &array(sig)).is_ok();
        assert_eq!(accepted, expected == "1", "{note}");
        // Verification still takes them: the check is the caller's to ask for.
        if note == "A=small-order,accept" {
            assert!(accepted);
            assert!(ed25519::is_small_order(&public), "{note}");
            torsion.insert(public);
        }
        // Non-canonical spellings of small-order points, which verify refuses anyway.
        if note == "A=y+p" || note == "A=sign-on-x0" || note == "A=y=p" {
            assert!(!accepted, "{note}");
            assert!(ed25519::is_small_order(&public), "{note}");
        }
        // y = p + 18 is 18, an ordinary y; and the ordinary key of the signature rows.
        if note == "A=y=2^255-1" || note == "baseline" {
            assert!(!ed25519::is_small_order(&public), "{note}");
        }
        count += 1;
    }
    assert_eq!(count, 28);
    assert_eq!(torsion.len(), 8);
}

fn set_coefficient(ek: &mut [u8], index: usize, value: u16) {
    let at = index / 2 * 3;
    if index.is_multiple_of(2) {
        ek[at] = value as u8;
        ek[at + 1] = (ek[at + 1] & 0xf0) | (value >> 8) as u8;
    } else {
        ek[at + 1] = (ek[at + 1] & 0x0f) | ((value << 4) as u8);
        ek[at + 2] = (value >> 4) as u8;
    }
}

#[test]
fn mlkem_agrees_with_nist_acvp() {
    let mut seen = [0usize; 5];
    for row in rows(include_str!("ssh/mlkem768_acvp.txt")) {
        match row[..] {
            ["keygen", d, z, ek, dk] => {
                let (got_ek, got_dk) = mlkem768::keygen(&array(d), &array(z));
                assert_eq!(got_ek, hex(ek));
                assert_eq!(got_dk, hex(dk));
                seen[0] += 1;
            }
            ["encaps", ek, m, c, k] => {
                let (ct, ss) = mlkem768::encaps(&hex(ek), &array(m)).unwrap();
                assert_eq!(ct, hex(c));
                assert_eq!(ss.as_slice(), hex(k));
                seen[1] += 1;
            }
            ["decaps", dk, c, k, reason] => {
                let ss = mlkem768::decaps(&hex(dk), &hex(c)).unwrap();
                assert_eq!(ss.as_slice(), hex(k), "{reason}");
                seen[2] += 1;
            }
            ["ekcheck", ek, pass, reason] => {
                assert_eq!(
                    mlkem768::encaps(&hex(ek), &[9; 32]).is_some(),
                    pass == "1",
                    "{reason}"
                );
                seen[3] += 1;
            }
            ["dkcheck", dk, pass, reason] => {
                assert_eq!(
                    mlkem768::decaps(&hex(dk), &[0; 1088]).is_some(),
                    pass == "1",
                    "{reason}"
                );
                seen[4] += 1;
            }
            _ => panic!("row"),
        }
    }
    assert_eq!(seen, [4, 4, 6, 10, 10]);
}

#[test]
fn mlkem_agrees_with_openssl_including_implicit_rejection() {
    let mut count = 0;
    for row in rows(include_str!("ssh/mlkem768_openssl.txt")) {
        let [d, z, m, ek_hash, dk_hash, ct_hash, ss, at, mask, rejected] = row[..] else {
            panic!("row")
        };
        let z: [u8; 32] = array(z);
        let (ek, dk) = mlkem768::keygen(&array(d), &z);
        assert_eq!(ek.len(), mlkem768::EK_LEN);
        assert_eq!(dk.len(), mlkem768::DK_LEN);
        assert_eq!(sha3_256(&ek).as_slice(), hex(ek_hash));
        assert_eq!(sha3_256(&dk).as_slice(), hex(dk_hash));
        let (mut ct, shared) = mlkem768::encaps(&ek, &array(m)).unwrap();
        assert_eq!(ct.len(), mlkem768::CT_LEN);
        assert_eq!(sha3_256(&ct).as_slice(), hex(ct_hash));
        assert_eq!(shared.as_slice(), hex(ss));
        assert_eq!(mlkem768::decaps(&dk, &ct), Some(shared));

        ct[at.parse::<usize>().unwrap()] ^= hex(mask)[0];
        let refused = mlkem768::decaps(&dk, &ct).unwrap();
        assert_eq!(refused.as_slice(), hex(rejected));
        assert_ne!(refused, shared);
        // FIPS 203 algorithm 18 line 7: J(z || c).
        let mut xof = Shake256::new();
        xof.absorb(&z);
        xof.absorb(&ct);
        let mut expected = [0u8; 32];
        xof.squeeze(&mut expected);
        assert_eq!(refused, expected);
        count += 1;
    }
    assert_eq!(count, 16);
}

#[test]
fn mlkem_round_trip_and_length_refusals() {
    for byte in 0..8u8 {
        let (ek, dk) = mlkem768::keygen(&[byte; 32], &[byte ^ 0x55; 32]);
        let (ct, ss) = mlkem768::encaps(&ek, &[byte ^ 0xaa; 32]).unwrap();
        assert_eq!(mlkem768::decaps(&dk, &ct), Some(ss));
        assert_eq!(mlkem768::SS_LEN, ss.len());

        assert!(mlkem768::encaps(&ek[..1183], &[0; 32]).is_none());
        assert!(mlkem768::encaps(&[ek.as_slice(), &[0]].concat(), &[0; 32]).is_none());
        assert!(mlkem768::encaps(&[], &[0; 32]).is_none());
        assert!(mlkem768::decaps(&dk[..2399], &ct).is_none());
        assert!(mlkem768::decaps(&[dk.as_slice(), &[0]].concat(), &ct).is_none());
        assert!(mlkem768::decaps(&dk, &ct[..1087]).is_none());
        assert!(mlkem768::decaps(&dk, &[ct.as_slice(), &[0]].concat()).is_none());
        assert!(mlkem768::decaps(&[], &[]).is_none());
    }
}

#[test]
fn mlkem_modulus_check_refuses_q_and_above_anywhere() {
    let (ek, _) = mlkem768::keygen(&[3; 32], &[4; 32]);
    for index in [0usize, 1, 2, 255, 256, 511, 512, 767] {
        for (value, valid) in [(3328u16, true), (3329, false), (4095, false)] {
            let mut changed = ek.clone();
            set_coefficient(&mut changed, index, value);
            assert_eq!(
                mlkem768::encaps(&changed, &[1; 32]).is_some(),
                valid,
                "{index} {value}"
            );
        }
    }
    // rho is not range-checked.
    let mut changed = ek.clone();
    changed[1152..].fill(0xff);
    assert!(mlkem768::encaps(&changed, &[1; 32]).is_some());
}

#[test]
fn mlkem_decaps_refuses_a_dk_whose_hash_does_not_match() {
    let (_, dk) = mlkem768::keygen(&[5; 32], &[6; 32]);
    let ct = [0x11u8; 1088];
    assert!(mlkem768::decaps(&dk, &ct).is_some());
    for at in [1152usize, 2335, 2336, 2367] {
        let mut changed = dk.clone();
        changed[at] ^= 1;
        assert!(mlkem768::decaps(&changed, &ct).is_none(), "{at}");
    }
    // Neither s nor z enters the hash.
    for at in [0usize, 1151, 2368, 2399] {
        let mut changed = dk.clone();
        changed[at] ^= 1;
        assert!(mlkem768::decaps(&changed, &ct).is_some(), "{at}");
    }
}

#[test]
fn bcrypt_pbkdf_agrees_with_openbsd_regress() {
    let mut count = 0;
    for row in rows(include_str!("ssh/bcrypt_pbkdf.txt")) {
        let [password, salt, rounds, expected] = row[..] else {
            panic!("row")
        };
        let expected = hex(expected);
        let mut out = vec![0u8; expected.len()];
        bcrypt_pbkdf(
            &hex(password),
            &hex(salt),
            rounds.parse().unwrap(),
            &mut out,
        )
        .unwrap();
        assert_eq!(out, expected, "{rounds}");
        count += 1;
    }
    assert_eq!(count, 9);
}

#[test]
fn bcrypt_pbkdf_refuses_its_documented_bounds() {
    let mut out = [0u8; 8];
    assert_eq!(bcrypt_pbkdf(b"", b"salt", 1, &mut out), Err(Refused));
    assert_eq!(bcrypt_pbkdf(b"pass", b"", 1, &mut out), Err(Refused));
    assert_eq!(bcrypt_pbkdf(b"pass", b"salt", 0, &mut out), Err(Refused));
    assert_eq!(bcrypt_pbkdf(b"pass", b"salt", 1, &mut []), Err(Refused));
    assert_eq!(out, [0u8; 8]);
    let mut long = vec![0u8; 1025];
    assert_eq!(bcrypt_pbkdf(b"pass", b"salt", 1, &mut long), Err(Refused));
    assert!(long.iter().all(|byte| *byte == 0));
}

/// Output byte i is byte i / stride of block i % stride, the "pbkdf2 deviation" of
/// OpenBSD's bcrypt_pbkdf.c. Each row is a whole output of a length with no OpenBSD
/// vector, from the reference in `ssh_vectors.py`, so every block's every byte is held
/// to the place it goes.
#[test]
fn bcrypt_pbkdf_output_layout_holds_in_every_lane() {
    let mut count = 0;
    let mut longest = 0;
    for row in rows(include_str!("ssh/bcrypt_layout.txt")) {
        let [password, salt, rounds, expected] = row[..] else {
            panic!("row")
        };
        let expected = hex(expected);
        let mut out = vec![0u8; expected.len()];
        bcrypt_pbkdf(
            &hex(password),
            &hex(salt),
            rounds.parse().unwrap(),
            &mut out,
        )
        .unwrap();
        assert_eq!(out, expected, "{rounds} {}", expected.len());
        longest = longest.max(expected.len());
        count += 1;
    }
    assert_eq!(count, 11);
    assert_eq!(longest, 1024);
}

fn base64(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut accumulator, mut bits) = (0u32, 0u32);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            other => panic!("{other}"),
        };
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    out
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> &'a [u8] {
        let (head, tail) = self.0.split_at(length);
        self.0 = tail;
        head
    }

    fn u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take(4).try_into().unwrap())
    }

    fn string(&mut self) -> &'a [u8] {
        let length = self.u32() as usize;
        self.take(length)
    }
}

struct KeyFile {
    passphrase: &'static str,
    public: Vec<u8>,
    cipher: Vec<u8>,
    salt: Vec<u8>,
    rounds: u32,
    encrypted: Vec<u8>,
    tag: Vec<u8>,
}

fn key_file(text: &'static str) -> KeyFile {
    let passphrase = text
        .lines()
        .find_map(|line| line.strip_prefix("# Test fixture, not a real identity. Passphrase: "))
        .unwrap();
    let body: String = text
        .lines()
        .filter(|line| !line.starts_with('#') && !line.starts_with('-'))
        .collect();
    let decoded = base64(&body);
    let mut file = Reader(&decoded);
    assert_eq!(file.take(15), b"openssh-key-v1\0");
    let cipher = file.string().to_vec();
    assert_eq!(file.string(), b"bcrypt");
    let mut options = Reader(file.string());
    let salt = options.string().to_vec();
    let rounds = options.u32();
    assert_eq!(file.u32(), 1);
    let public_blob = file.string();
    let mut public = Reader(public_blob);
    assert_eq!(public.string(), b"ssh-ed25519");
    let public = public.string().to_vec();
    let encrypted = file.string().to_vec();
    let tag = file.0.to_vec();
    KeyFile {
        passphrase,
        public,
        cipher,
        salt,
        rounds,
        encrypted,
        tag,
    }
}

/// A decrypted `openssh-key-v1` private section must hold ssh-keygen's own check
/// integers, the key it printed and a seed whose public key is that one.
fn check_private_section(plain: &[u8], public: &[u8], comment: &[u8]) {
    let mut private = Reader(plain);
    let check = private.u32();
    assert_eq!(private.u32(), check);
    assert_eq!(private.string(), b"ssh-ed25519");
    assert_eq!(private.string(), public);
    let secret = private.string();
    assert_eq!(secret.len(), 64);
    assert_eq!(&secret[32..], public);
    assert_eq!(private.string(), comment);
    assert!(private.0.iter().zip(1u8..).all(|(byte, n)| *byte == n));
    let seed: [u8; 32] = secret[..32].try_into().unwrap();
    let signing = SigningKey::from_seed(&seed).unwrap();
    assert_eq!(signing.public_key().as_slice(), public);
}

/// ssh-keygen's file, decrypted with bcrypt_pbkdf and AES-256-CTR.
fn decrypts(text: &'static str, comment: &[u8]) {
    let file = key_file(text);
    assert_eq!(file.cipher, b"aes256-ctr");
    let mut material = [0u8; 48];
    bcrypt_pbkdf(
        file.passphrase.as_bytes(),
        &file.salt,
        file.rounds,
        &mut material,
    )
    .unwrap();
    let key: [u8; 32] = material[..32].try_into().unwrap();
    let iv: [u8; 16] = material[32..].try_into().unwrap();
    let mut plain = file.encrypted.clone();
    // Split across calls and block edges: the keystream must not care.
    let mut ctr = Aes256Ctr::new(&key, &iv);
    let (head, tail) = plain.split_at_mut(21);
    ctr.apply(head);
    ctr.apply(tail);
    check_private_section(&plain, &file.public, comment);
    assert!(file.tag.is_empty());
}

#[test]
fn openssh_key_files_decrypt_through_bcrypt_and_aes() {
    decrypts(
        include_str!("ssh/openssh_ed25519_a2.txt"),
        b"td-primitives rounds 2",
    );
    decrypts(
        include_str!("ssh/openssh_ed25519_a16.txt"),
        b"td-primitives rounds 16 with a longer comment than usual",
    );
}

/// OpenSSH encrypts a key file under chacha20-poly1305@openssh.com with no length:
/// sequence number 0, the whole section from counter 1 under K_2, the tag over it
/// bare. That holds K_2, the payload's counter and the MAC's short last block to real
/// OpenSSH output, and a second stride of bcrypt_pbkdf at 64 bytes. `open` keeps its
/// first 4 bytes for a length this file does not have, so the keystream is read through
/// `seal`, over zero length bytes, whose payload lines up with the section's start.
#[test]
fn an_openssh_chacha_key_file_decrypts_in_full() {
    let file = key_file(include_str!("ssh/openssh_ed25519_chachapoly.txt"));
    assert_eq!(file.cipher, b"chacha20-poly1305@openssh.com");
    let mut material = [0u8; 64];
    bcrypt_pbkdf(
        file.passphrase.as_bytes(),
        &file.salt,
        file.rounds,
        &mut material,
    )
    .unwrap();
    let tag: [u8; 16] = file.tag.as_slice().try_into().unwrap();
    let aead = OpenSshAead::new(&material);
    let mut sealed = file.encrypted.clone();
    aead.open(0, &mut sealed, &tag).unwrap();

    let mut packet = vec![0u8; 4];
    packet.extend_from_slice(&file.encrypted);
    aead.seal(0, &mut packet).unwrap();
    check_private_section(&packet[4..], &file.public, CHACHA_FILE_COMMENT);

    let mut wrong = material;
    wrong[1] ^= 1;
    let mut sealed = file.encrypted.clone();
    assert!(OpenSshAead::new(&wrong).open(0, &mut sealed, &tag).is_err());
    let mut sealed = file.encrypted.clone();
    assert!(aead.open(1, &mut sealed, &tag).is_err());
}

const CHACHA_FILE_COMMENT: &[u8] = b"td-primitives chachapoly";

/// A random key almost never passes the modulus check, so it stops there. The largest
/// valid coefficients are what reach the arithmetic with the most to carry.
#[test]
fn the_largest_valid_peer_key_encapsulates() {
    let mut largest = Vec::with_capacity(mlkem768::EK_LEN);
    while largest.len() < mlkem768::EK_LEN - 32 {
        largest.extend_from_slice(&[0x00, 0x0d, 0xd0]);
    }
    largest.extend_from_slice(&[0xff; 32]);
    assert!(mlkem768::encaps(&largest, &[7; 32]).is_some());
    // One past, in the first coefficient: 3329.
    let mut over = largest.clone();
    over[0] = 0x01;
    assert!(mlkem768::encaps(&over, &[7; 32]).is_none());
}

/// Under overflow checks in a debug run: arbitrary bytes through `is_small_order`, ML-KEM
/// and the OpenSSH AEAD. A random encapsulation key stops at its modulus check; a test
/// above and the corpora reach what lies past it. Only "returns at all" is asserted,
/// beyond what must hold.
#[test]
fn arbitrary_inputs_do_not_panic() {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut fill = |out: &mut [u8]| {
        for byte in out.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
    };
    let (ek, dk) = mlkem768::keygen(&[8; 32], &[9; 32]);
    for round in 0..64 {
        let mut a = [0u8; 32];
        fill(&mut a);
        let _ = ed25519::is_small_order(&a);

        let mut ct = vec![0u8; mlkem768::CT_LEN];
        fill(&mut ct);
        assert!(mlkem768::decaps(&dk, &ct).is_some());
        let mut random_ek = vec![0u8; mlkem768::EK_LEN];
        fill(&mut random_ek);
        let _ = mlkem768::encaps(&random_ek, &a);
        let (ct, ss) = mlkem768::encaps(&ek, &a).unwrap();
        assert_eq!(mlkem768::decaps(&dk, &ct), Some(ss));

        let mut packet = vec![0u8; round * 7];
        fill(&mut packet);
        let mut tag = [0u8; 16];
        fill(&mut tag);
        let mut key = [0u8; 64];
        fill(&mut key);
        assert!(OpenSshAead::new(&key)
            .open(round as u32, &mut packet, &tag)
            .is_err());
    }
}

#[test]
fn aes256_ctr_agrees_with_openssl_across_counter_wraps() {
    let mut count = 0;
    for row in rows(include_str!("ssh/aes256ctr.txt")) {
        let [key, iv, stream] = row[..] else {
            panic!("row")
        };
        let key: [u8; 32] = array(key);
        let iv: [u8; 16] = array(iv);
        let stream = hex(stream);
        let mut out = vec![0u8; stream.len()];
        Aes256Ctr::new(&key, &iv).apply(&mut out);
        assert_eq!(out, stream);
        // Every split point continues the same keystream.
        for at in 0..=stream.len() {
            let mut out = vec![0u8; stream.len()];
            let mut ctr = Aes256Ctr::new(&key, &iv);
            let (head, tail) = out.split_at_mut(at);
            ctr.apply(head);
            ctr.apply(&mut []);
            ctr.apply(tail);
            assert_eq!(out, stream, "{at}");
        }
        count += 1;
    }
    assert_eq!(count, 60);
}
