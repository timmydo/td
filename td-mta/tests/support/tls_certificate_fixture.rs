//! Local synthetic certificates shared by transport and allocation fixtures.
#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use td_crypto::{Crypto, P256Key, Provider};

fn der(tag: u8, data: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    if data.len() < 128 {
        out.push(data.len() as u8);
    } else {
        let length = data.len().to_be_bytes();
        let length = length
            .iter()
            .skip_while(|byte| **byte == 0)
            .copied()
            .collect::<Vec<_>>();
        out.push(0x80 | length.len() as u8);
        out.extend_from_slice(&length);
    }
    out.extend_from_slice(data);
    out
}
fn seq(parts: &[Vec<u8>]) -> Vec<u8> {
    der(0x30, &parts.concat())
}
fn oid(bytes: &[u8]) -> Vec<u8> {
    der(6, bytes)
}
fn name(bytes: &[u8]) -> Vec<u8> {
    seq(&[der(0x31, &seq(&[oid(&[0x55, 4, 3]), der(0x0c, bytes)]))])
}
fn extension(id: u8, critical: bool, body: Vec<u8>) -> Vec<u8> {
    let mut parts = vec![oid(&[0x55, 0x1d, id])];
    if critical {
        parts.push(der(1, &[0xff]));
    }
    parts.push(der(4, &body));
    seq(&parts)
}
fn integer(bytes: &[u8]) -> Vec<u8> {
    let mut value = bytes
        .iter()
        .skip_while(|byte| **byte == 0)
        .copied()
        .collect::<Vec<_>>();
    if value.is_empty() || value[0] & 0x80 != 0 {
        value.insert(0, 0);
    }
    der(2, &value)
}
pub(crate) fn certificate_names(
    key: &P256Key,
    signer: &P256Key,
    ca: bool,
    serial: u8,
    names: &[&str],
    client: bool,
) -> Vec<u8> {
    certificate_with(
        key,
        signer,
        &Certificate {
            ca,
            serial,
            names,
            client,
            issuer: b"local-test-root",
            subject: if ca { b"local-test-root" } else { b"localhost" },
            padding: 0,
        },
    )
}

pub(crate) struct Certificate<'a> {
    pub ca: bool,
    pub serial: u8,
    pub names: &'a [&'a str],
    pub client: bool,
    pub issuer: &'a [u8],
    pub subject: &'a [u8],
    pub padding: usize,
}

pub(crate) fn certificate_with(
    key: &P256Key,
    signer: &P256Key,
    certificate: &Certificate<'_>,
) -> Vec<u8> {
    let Certificate {
        ca,
        serial,
        names,
        client,
        issuer,
        subject,
        padding,
    } = *certificate;
    let provider = Provider;
    let mut public = [0; 65];
    provider.p256_public(key, &mut public).unwrap();
    let spki = seq(&[
        seq(&[
            oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 2, 1]),
            oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 3, 1, 7]),
        ]),
        der(3, &[&[0][..], &public].concat()),
    ]);
    let algorithm = seq(&[oid(&[0x2a, 0x86, 0x48, 0xce, 0x3d, 4, 3, 2])]);
    let mut extensions = vec![
        extension(
            0x13,
            true,
            if ca {
                seq(&[der(1, &[0xff])])
            } else {
                seq(&[])
            },
        ),
        extension(0x0f, true, der(3, if ca { &[1, 6] } else { &[7, 0x80] })),
    ];
    if !ca {
        extensions.push(extension(
            0x11,
            false,
            seq(&names
                .iter()
                .map(|name| der(0x82, name.as_bytes()))
                .collect::<Vec<_>>()),
        ));
        extensions.push(extension(
            0x25,
            false,
            seq(&[oid(&[0x2b, 6, 1, 5, 5, 7, 3, if client { 2 } else { 1 }])]),
        ));
    }
    if padding != 0 {
        // An unknown noncritical extension carries bounded fixture padding.
        extensions.push(seq(&[oid(&[0x2a, 3, 4]), der(4, &vec![0; padding])]));
    }
    let body = seq(&[
        der(0xa0, &der(2, &[2])),
        der(2, &[serial]),
        algorithm.clone(),
        name(issuer),
        seq(&[der(0x17, b"250101000000Z"), der(0x17, b"350101000000Z")]),
        name(subject),
        spki,
        der(0xa3, &seq(&extensions)),
    ]);
    let mut signature = [0; 64];
    provider.sign_es256(signer, &body, &mut signature).unwrap();
    let signature = seq(&[integer(&signature[..32]), integer(&signature[32..])]);
    seq(&[body, algorithm, der(3, &[&[0][..], &signature].concat())])
}
pub(crate) fn pem(label: &str, bytes: &[u8]) -> Vec<u8> {
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = format!("-----BEGIN {label}-----\n").into_bytes();
    let mut count = 0;
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for (index, shift) in [18, 12, 6, 0].iter().enumerate() {
            output.push(if index > chunk.len() {
                b'='
            } else {
                alphabet[((value >> shift) & 63) as usize]
            });
            count += 1;
            if count % 64 == 0 {
                output.push(b'\n');
            }
        }
    }
    if count % 64 != 0 {
        output.push(b'\n');
    }
    output.extend_from_slice(format!("-----END {label}-----\n").as_bytes());
    output
}
