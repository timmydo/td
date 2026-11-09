#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{Crypto, Provider, P256_PKCS8_CAPACITY};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivatePkcs8KeyDer};

fn encode(bytes: &[u8]) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = Vec::new();
    for chunk in bytes.chunks(3) {
        let word = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for shift in [18, 12, 6, 0] {
            output.push(ALPHABET[((word >> shift) & 63) as usize]);
        }
        for index in 0..3 - chunk.len() {
            let offset = output.len() - 1 - index;
            output[offset] = b'=';
        }
    }
    output
}

pub(crate) fn pem(label: &str, der: &[u8]) -> Vec<u8> {
    let mut output = format!("-----BEGIN {label}-----\n").into_bytes();
    for line in encode(der).chunks(64) {
        output.extend_from_slice(line);
        output.push(b'\n');
    }
    output.extend_from_slice(format!("-----END {label}-----\n").as_bytes());
    output
}

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    match content.len() {
        0..=127 => out.push(content.len() as u8),
        128..=255 => out.extend_from_slice(&[0x81, content.len() as u8]),
        _ => {
            out.push(0x82);
            out.extend_from_slice(&u16::try_from(content.len()).unwrap().to_be_bytes());
        }
    }
    out.extend_from_slice(content);
    out
}

fn sequence(content: &[u8]) -> Vec<u8> {
    tlv(0x30, content)
}

#[test]
fn canonical_base64_and_transactional_output() {
    for (text, bytes) in [
        (&b"TQ=="[..], &b"M"[..]),
        (b"TWE=", b"Ma"),
        (b"TWFu", b"Man"),
        (b"T Q= =", b"M"),
        (b"T\tW\r\nE=", b"Ma"),
        (b" /+7d \r\n", &[255, 238, 221]),
    ] {
        let mut out = [0xa5; 8];
        let length = decode(text, &mut out).unwrap();
        assert_eq!(&out[..length], bytes);
        assert_eq!(&out[length..], &[0xa5; 8][length..]);
    }
    for text in [
        &b"TR=="[..],
        b"TWF=",
        b"TQ=",
        b"TQ===",
        b"TQ==TWFu",
        b"=Q==",
        b"TQ-_",
        b"",
        b" \r\n",
        b"TQ==\0",
        b"TQ==\r",
        b"T\rQ==",
        b"TQ==\x0b",
        b"TQ==\x0c",
        b"TWFu!AAA",
    ] {
        let mut out = [0xa5; 8];
        assert_eq!(decode(text, &mut out), Err(Error::Invalid), "{text:?}");
        assert_eq!(out, [0xa5; 8]);
    }
    let mut out = [0xa5; 2];
    assert_eq!(decode(b"TWFu", &mut out), Err(Error::Capacity));
    assert_eq!(out, [0xa5; 2]);
    // Every nonzero unused padding bit must be rejected.
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for &value in alphabet.iter().take(16).skip(1) {
        assert_eq!(
            inspect(&[b'A', value, b'=', b'=']).err(),
            Some(Error::Invalid)
        );
    }
    for &value in alphabet.iter().take(4).skip(1) {
        assert_eq!(
            inspect(&[b'A', b'A', value, b'=']).err(),
            Some(Error::Invalid)
        );
    }
}

#[test]
fn certificate_decoding_matches_independent_pem_reader() {
    for length in (1..260).chain([1024, 4096, CERTIFICATE_DER_CAPACITY - 4]) {
        let content: Vec<_> = (0..length).map(|i| i as u8).collect();
        let der = sequence(&content);
        let encoded = pem("CERTIFICATE", &der);
        for input in [
            encoded.clone(),
            String::from_utf8(encoded)
                .unwrap()
                .replace('\n', "\r\n")
                .into_bytes(),
        ] {
            let independent = CertificateDer::from_pem_slice(&input).unwrap();
            let mut reader = PemCertificates::chain(&input).unwrap();
            let mut out = vec![0xa5; der.len() + 7];
            assert_eq!(reader.remaining(), 1);
            let count = reader.decode_next(&mut out).unwrap().unwrap();
            assert_eq!(&out[..count], independent.as_ref());
            assert_eq!(&out[..count], der);
            assert_eq!(&out[count..], &[0xa5; 7]);
            assert_eq!(reader.remaining(), 0);
            let saved = out.clone();
            assert_eq!(reader.decode_next(&mut out), Ok(None));
            assert_eq!(out, saved);
        }
    }
}

#[test]
fn certificate_envelopes_and_retry() {
    let der = sequence(&[1, 2, 3, 4]);
    let encoded = pem("CERTIFICATE", &der);
    let indented = [b"   ".as_slice(), &encoded].concat();
    assert!(PemCertificates::chain(&indented).is_ok());
    let mut input = b" \t\r\n\x0b\x0c".to_vec();
    input.extend_from_slice(&encoded);
    input.extend_from_slice(b" \n\t");
    input.extend_from_slice(&encoded[..encoded.len() - 1]);
    let mut reader = PemCertificates::chain(&input).unwrap();
    assert_eq!(reader.remaining(), 2);
    assert_eq!(
        format!("{reader:?}"),
        "PemCertificates { remaining: 2, .. }"
    );
    for _ in 0..2 {
        let count = reader.remaining();
        for size in 0..der.len() {
            let mut out = vec![0xa5; size];
            assert_eq!(reader.decode_next(&mut out), Err(Error::Capacity));
            assert_eq!(reader.remaining(), count);
            assert!(out.iter().all(|&b| b == 0xa5));
        }
        let mut out = vec![0; der.len()];
        assert_eq!(reader.decode_next(&mut out), Ok(Some(der.len())));
        assert_eq!(out, der);
    }
    assert_eq!(reader.decode_next(&mut []), Ok(None));
    for bad in [
        Vec::new(),
        b" \t\n".to_vec(),
        b"garbage".to_vec(),
        [b"comment\n".as_slice(), &encoded].concat(),
        [encoded.as_slice(), b"trailing"].concat(),
        [
            encoded.as_slice(),
            b"-----BEGIN CERTIFICATE-----\n!\n-----END CERTIFICATE-----\n",
        ]
        .concat(),
        pem("PRIVATE KEY", &der),
        pem("X509 CERTIFICATE", &der),
        encoded
            .iter()
            .map(|&b| if b == b'\n' { b'\r' } else { b })
            .collect(),
        String::from_utf8(encoded.clone())
            .unwrap()
            .replace("-----END CERTIFICATE-----", "-----END PRIVATE KEY-----")
            .into_bytes(),
        String::from_utf8(encoded.clone())
            .unwrap()
            .replace(
                "-----BEGIN CERTIFICATE-----\n",
                "-----BEGIN CERTIFICATE-----\nProc-Type: 4,ENCRYPTED\n",
            )
            .into_bytes(),
        String::from_utf8(encoded.clone())
            .unwrap()
            .replace("-----END CERTIFICATE-----", " -----END CERTIFICATE-----")
            .into_bytes(),
    ] {
        assert_eq!(
            PemCertificates::chain(&bad).err(),
            Some(Error::Invalid),
            "{bad:?}"
        );
    }
    // Every proper truncation before the complete footer is invalid.
    for end in 0..encoded.len() - 1 {
        assert_eq!(
            PemCertificates::chain(&encoded[..end]).err(),
            Some(Error::Invalid)
        );
    }
}

#[test]
fn certificate_limits_and_der_envelopes() {
    let small = pem("CERTIFICATE", &sequence(&[1]));
    assert_eq!(
        PemCertificates::chain(&small.repeat(8))
            .unwrap()
            .remaining(),
        8
    );
    assert_eq!(
        PemCertificates::chain(&small.repeat(9)).err(),
        Some(Error::Invalid)
    );
    assert_eq!(
        PemCertificates::trust_bundle(&small.repeat(128))
            .unwrap()
            .remaining(),
        128
    );
    assert_eq!(
        PemCertificates::trust_bundle(&small.repeat(129)).err(),
        Some(Error::Invalid)
    );
    for (max, chain) in [(CHAIN_INPUT, true), (TRUST_INPUT, false)] {
        let mut input = small.clone();
        input.resize(max, b' ');
        let check = |input: &[u8]| {
            if chain {
                PemCertificates::chain(input).map(|_| ())
            } else {
                PemCertificates::trust_bundle(input).map(|_| ())
            }
        };
        assert_eq!(check(&input), Ok(()));
        input.push(b' ');
        assert_eq!(check(&input), Err(Error::Invalid));
    }
    let exact = pem(
        "CERTIFICATE",
        &sequence(&vec![0; CERTIFICATE_DER_CAPACITY - 4]),
    );
    assert!(PemCertificates::chain(&exact).is_ok());
    let over = pem(
        "CERTIFICATE",
        &sequence(&vec![0; CERTIFICATE_DER_CAPACITY - 3]),
    );
    assert_eq!(PemCertificates::chain(&over).err(), Some(Error::Invalid));
    assert_eq!(
        PemCertificates::trust_bundle(&over).err(),
        Some(Error::Invalid)
    );
    for der in [
        &b"\x30\x00"[..],
        b"\x31\x01\x00",
        b"\x30\x80\x00\x00",
        b"\x30\x81\x01\x00",
        b"\x30\x82\x00\x01\x00",
        b"\x30\x83\x00\x00\x01\x00",
        b"\x30\x01",
        b"\x30\x01\x00\x00",
        b"\x30\x82\x01\x00",
    ] {
        assert_eq!(
            PemCertificates::chain(&pem("CERTIFICATE", der)).err(),
            Some(Error::Invalid)
        );
    }
    // Exercise aggregate and count checks independently of the tighter text cap.
    let pair = small.repeat(2);
    assert_eq!(
        PemCertificates::new(&pair, pair.len(), 2, 5).err(),
        Some(Error::Invalid)
    );
    assert!(PemCertificates::new(&pair, pair.len(), 2, 6).is_ok());
}

#[test]
fn p256_pem_loading_and_refusals() {
    let mut bytes = [0; P256_PKCS8_CAPACITY];
    let length = Provider.generate_p256(&mut bytes).unwrap();
    let input = pem("PRIVATE KEY", &bytes[..length]);
    let oracle = PrivatePkcs8KeyDer::from_pem_slice(&input).unwrap();
    assert_eq!(oracle.secret_pkcs8_der(), &bytes[..length]);
    let expected = Provider.load_p256(&bytes[..length]).unwrap();
    let mut expected_public = [0; 65];
    Provider
        .p256_public(&expected, &mut expected_public)
        .unwrap();
    for mut text in [
        input.clone(),
        String::from_utf8(input.clone())
            .unwrap()
            .replace('\n', "\r\n")
            .into_bytes(),
    ] {
        text.resize(KEY_INPUT, b' ');
        let key = Provider.load_p256_pem(&text).unwrap();
        let mut public = [0; 65];
        Provider.p256_public(&key, &mut public).unwrap();
        assert_eq!(public, expected_public);
        text.push(b' ');
        assert_eq!(Provider.load_p256_pem(&text).err(), Some(Error::Invalid));
    }
    let mut trailing_der = bytes[..length].to_vec();
    trailing_der.push(0);
    for bad in [
        input.repeat(2),
        pem("EC PRIVATE KEY", &bytes[..length]),
        pem("ENCRYPTED PRIVATE KEY", &bytes[..length]),
        pem("CERTIFICATE", &bytes[..length]),
        pem("PRIVATE KEY", &trailing_der),
        pem("PRIVATE KEY", &[0; 151]),
        pem("PRIVATE KEY", &[0x30, 1, 0]),
    ] {
        assert_eq!(Provider.load_p256_pem(&bad).err(), Some(Error::Invalid));
    }
    for end in 0..input.len() - 1 {
        assert_eq!(
            Provider.load_p256_pem(&input[..end]).err(),
            Some(Error::Invalid)
        );
    }
    // Include both optional SEC1 fields to exercise the maximum admitted shape.
    let scalar = bytes[..length]
        .windows(2)
        .position(|pair| pair == [4, 32])
        .unwrap()
        + 2;
    let curve = tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]);
    let algorithm = sequence(
        &[
            tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]),
            curve.clone(),
        ]
        .concat(),
    );
    let inner = sequence(
        &[
            tlv(0x02, &[1]),
            tlv(0x04, &bytes[scalar..scalar + 32]),
            tlv(0xa0, &curve),
            tlv(0xa1, &tlv(0x03, &[&[0][..], &expected_public].concat())),
        ]
        .concat(),
    );
    let exact = sequence(&[tlv(0x02, &[0]), algorithm, tlv(0x04, &inner)].concat());
    assert_eq!(exact.len(), P256_PKCS8_CAPACITY);
    let exact_pem = pem("PRIVATE KEY", &exact);
    assert_eq!(
        PrivatePkcs8KeyDer::from_pem_slice(&exact_pem)
            .unwrap()
            .secret_pkcs8_der(),
        exact
    );
    let key = Provider.load_p256_pem(&exact_pem).unwrap();
    let mut public = [0; 65];
    Provider.p256_public(&key, &mut public).unwrap();
    assert_eq!(public, expected_public);
    // Scalar zero is structurally valid but refused by the key provider.
    bytes[scalar..scalar + 32].fill(0);
    assert_eq!(
        Provider
            .load_p256_pem(&pem("PRIVATE KEY", &bytes[..length]))
            .err(),
        Some(Error::Invalid)
    );
    // Refused callbacks publish no key; Drop delegates to the same clearing helper.
    assert_eq!(
        private_key(&input, |_| Err::<(), _>(Error::Crypto)),
        Err(Error::Crypto)
    );
    let mut secret = Secret([0xa5; P256_PKCS8_CAPACITY]);
    secret.clear();
    assert_eq!(secret.0, [0; P256_PKCS8_CAPACITY]);
}

/// The branching decoder this file replaced, kept as the differential
/// reference for the branch-free one.
fn reference_sextet(byte: u8) -> Result<u8, Error> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(Error::Invalid),
    }
}

fn reference_base64(input: &[u8]) -> Result<Vec<u8>, Error> {
    let mut previous_cr = false;
    for &byte in input {
        if previous_cr && byte != b'\n' {
            return Err(Error::Invalid);
        }
        previous_cr = byte == b'\r';
    }
    if previous_cr {
        return Err(Error::Invalid);
    }
    let mut input = input
        .iter()
        .copied()
        .filter(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'));
    let mut out = Vec::new();
    while let Some(first) = input.next() {
        let a = reference_sextet(first)?;
        let b = reference_sextet(input.next().ok_or(Error::Invalid)?)?;
        let third = input.next().ok_or(Error::Invalid)?;
        let fourth = input.next().ok_or(Error::Invalid)?;
        let (c, d, count) = match (third, fourth) {
            (b'=', b'=') if b & 15 == 0 => (0, 0, 1),
            (b'=', _) => return Err(Error::Invalid),
            (_, b'=') => {
                let c = reference_sextet(third)?;
                if c & 3 != 0 {
                    return Err(Error::Invalid);
                }
                (c, 0, 2)
            }
            _ => (reference_sextet(third)?, reference_sextet(fourth)?, 3),
        };
        let decoded = [(a << 2) | (b >> 4), (b << 4) | (c >> 2), (c << 6) | d];
        out.extend_from_slice(&decoded[..count]);
        if count != 3 && input.next().is_some() {
            return Err(Error::Invalid);
        }
    }
    if out.is_empty() {
        return Err(Error::Invalid);
    }
    Ok(out)
}

fn branch_free_base64(input: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    let length = base64(input, |bytes| {
        out.extend_from_slice(bytes);
        Ok(())
    })?;
    assert_eq!(length, out.len());
    Ok(out)
}

#[test]
fn branch_free_sextet_matches_every_byte() {
    for byte in 0..=255u8 {
        let (value, valid) = sextet(byte);
        match reference_sextet(byte) {
            Ok(expected) => assert_eq!((value, valid), (expected, 0xff), "{byte}"),
            Err(_) => assert_eq!(valid, 0, "{byte}"),
        }
    }
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    for (index, &byte) in alphabet.iter().enumerate() {
        assert_eq!(sextet(byte), (index as u8, 0xff));
    }
}

#[test]
fn branch_free_base64_classes_padding_and_refusals() {
    type Case = (&'static [u8], Result<&'static [u8], Error>);
    let cases: &[Case] = &[
        (b"AZaz09+/", Ok(&[0x01, 0x96, 0xb3, 0xd3, 0xdf, 0xbf])),
        (b"Zg==", Ok(b"f")),
        (b"Zm8=", Ok(b"fo")),
        (b"Zm9v\r\nYg==", Ok(b"foob")),
        (b" Zm9v\tYmFy\n", Ok(b"foobar")),
        (b"", Err(Error::Invalid)),
        (b"Zh==", Err(Error::Invalid)),
        (b"Zm9=", Err(Error::Invalid)),
        (b"Zg=a", Err(Error::Invalid)),
        (b"Z===", Err(Error::Invalid)),
        (b"Zg==Zg==", Err(Error::Invalid)),
        (b"Zm9", Err(Error::Invalid)),
        (b"Zm9-", Err(Error::Invalid)),
        (b"Zm9_", Err(Error::Invalid)),
        (b"Zm\x009", Err(Error::Invalid)),
        (b"Zm\xc39", Err(Error::Invalid)),
        (b"Zm9v\rYg==", Err(Error::Invalid)),
        (b"Zm9v\r", Err(Error::Invalid)),
    ];
    for (input, expected) in cases {
        let expected = expected.map(<[u8]>::to_vec);
        assert_eq!(branch_free_base64(input), expected, "{input:?}");
        assert_eq!(reference_base64(input), expected, "{input:?}");
    }
}

#[test]
fn branch_free_base64_matches_the_reference_on_random_input() {
    // Inputs weighted toward the alphabet, padding and whitespace, so most
    // reach the later checks, plus arbitrary bytes.
    let symbols = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/==\r\n \t-_.";
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut accepted = 0;
    for round in 0..20_000 {
        let length = (next() % 24) as usize;
        let input: Vec<u8> = (0..length)
            .map(|_| {
                let draw = next();
                if round % 4 == 3 && draw % 9 == 0 {
                    (draw >> 8) as u8
                } else {
                    symbols[(draw >> 8) as usize % symbols.len()]
                }
            })
            .collect();
        let expected = reference_base64(&input);
        accepted += usize::from(expected.is_ok());
        assert_eq!(branch_free_base64(&input), expected, "{input:?}");
    }
    // Valid encodings of random bytes, where the two must agree on output.
    for _ in 0..2_000 {
        let length = 1 + (next() % 48) as usize;
        let bytes: Vec<u8> = (0..length).map(|_| next() as u8).collect();
        let encoded = encode(&bytes);
        assert_eq!(branch_free_base64(&encoded).unwrap(), bytes);
        assert_eq!(reference_base64(&encoded).unwrap(), bytes);
    }
    assert!(accepted > 100, "{accepted}");
}
