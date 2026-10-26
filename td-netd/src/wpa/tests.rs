#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

use super::*;
use crate::genl::Bss;

// From tests/wpa_vectors.py: an authenticator written apart from this
// module (hashlib, hmac and `cryptography`'s RFC 3394 wrap). Passphrase
// "correct horse battery", SSID "td-test", AA 02:00:00:00:00:01, SPA
// 02:00:00:00:00:02, ANonce 0x10..0x2f, SNonce 0x40..0x5f, GTK 0xa0..0xaf at
// index 1 with RSC 5, then GTK 0xc0..0xcf at index 2 with RSC 9.
const PMK: &str = "f19859fa67d0e6e157b1ee33b92610aa7f3dfa3ddd400d41162b3591bf2e5508";
const PTK: &str = "318123a4c614043643ff1504d3ef9acc17131817b181000064a6e828e25024ba9ec055cf586c3f1e6f85bcb3e9d477dc";
const M1: &str = "0203005f02008a00100000000000000001101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
const M2: &str = "0203007502010a00000000000000000001404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f00000000000000000000000000000000000000000000000000000000000000000bc0f9d92bacd3341e3cb42936d21e85001630140100000fac040100000fac040100000fac020000";
const M3: &str = "020300970213ca00100000000000000002101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f00000000000000000000000000000000050000000000000000000000000000007c41a55f8de1e34200e5c5f3ce367ba6003839432598a5ba766bcc1c5b3e8a4220495c5caf6aeb007bf087b0282d95f6658300b8002f50ff2f2e5739553a22e09755d9682f5bc4677674";
const M4: &str = "0203005f02030a000000000000000000020000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000048d0961a56ab75d31941138063925ef40000";
const G1: &str = "0203007f02138200000000000000000003000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000090000000000000000000000000000004d4d00d23563de9b5891cd4018418f3b0020868a4d6aac93bee28e97f0e0d9b17d2aa457156a2fd2b563f0a39a35cc058f34";
const G2: &str = "0203005f0203020000000000000000000300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000cf9032c80011d56b735351fed8c3c5860000";
/// IEEE 802.11-2020 J.3.2 test case 1's PRF-512.
const PRF_PREFIX_HI_THERE: &str = "bcd4c650b30b9684951829e0d75f9d54b862175ed9f00606e17d8da35402ffee75df78c3d31e0f889f012120c0862beb67753e7439ae242edb8373698356cf5a";
const AP_RSN: &str = "30140100000fac040100000fac040100000fac020c00";

const AA: [u8; 6] = [2, 0, 0, 0, 0, 1];
const SPA: [u8; 6] = [2, 0, 0, 0, 0, 2];

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

fn array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    bytes.try_into().unwrap()
}

fn snonce() -> [u8; 32] {
    array(&(0x40..0x60).collect::<Vec<u8>>())
}

fn ap_rsn() -> Rsn {
    parse_rsn(&hex(AP_RSN)[2..]).unwrap()
}

fn supplicant() -> Supplicant {
    Supplicant::new(array(&hex(PMK)), AA, SPA, ap_rsn())
}

#[test]
fn prf_matches_ieee_test_vector() {
    let mut out = [0u8; 64];
    prf(&[0x0b; 20], b"prefix", b"Hi There", &mut out);
    assert_eq!(out.to_vec(), hex(PRF_PREFIX_HI_THERE));
}

#[test]
fn passphrase_and_ptk_match_the_authenticator() {
    let mut pmk = [0u8; 32];
    crate::sha1::pbkdf2_hmac_sha1(b"correct horse battery", b"td-test", 4096, &mut pmk).unwrap();
    assert_eq!(pmk.to_vec(), hex(PMK));
    let anonce = array(&(0x10..0x30).collect::<Vec<u8>>());
    let ptk = derive_ptk(&pmk, &AA, &SPA, &anonce, &snonce());
    let expected = hex(PTK);
    assert_eq!(ptk.kck.to_vec(), expected[..16]);
    assert_eq!(ptk.kek.to_vec(), expected[16..32]);
    assert_eq!(ptk.tk.to_vec(), expected[32..]);
    // The derivation orders addresses and nonces, so either side agrees.
    let swapped = derive_ptk(&pmk, &SPA, &AA, &snonce(), &anonce);
    assert_eq!(swapped.tk, ptk.tk);
}

#[test]
fn four_way_and_group_handshakes_produce_the_authenticators_frames() {
    let mut station = supplicant();
    let actions = station.receive(&hex(M1), true, snonce()).unwrap();
    assert_eq!(
        actions,
        vec![Action::Send {
            frame: hex(M2),
            encrypt: false
        }]
    );
    let actions = station.receive(&hex(M3), true, [0xff; 32]).unwrap();
    let tk: [u8; 16] = array(&hex(PTK)[32..]);
    assert_eq!(
        actions,
        vec![
            Action::Send {
                frame: hex(M4),
                encrypt: false
            },
            Action::PairwiseKey { tk },
            Action::GroupKey {
                index: 1,
                key: (0xa0..0xb0).collect(),
                rsc: [5, 0, 0, 0, 0, 0]
            },
            Action::Authorize,
        ]
    );
    let actions = station.receive(&hex(G1), true, [0xff; 32]).unwrap();
    assert_eq!(
        actions,
        vec![
            Action::GroupKey {
                index: 2,
                key: (0xc0..0xd0).collect(),
                rsc: [9, 0, 0, 0, 0, 0]
            },
            Action::Send {
                frame: hex(G2),
                encrypt: true
            },
        ]
    );
    // Each is replayed: refused.
    for frame in [M1, M3, G1] {
        assert!(
            station.receive(&hex(frame), true, snonce()).is_err(),
            "{frame}"
        );
    }
}

#[test]
fn any_changed_bit_in_message_3_is_refused() {
    let m3 = hex(M3);
    for bit in 0..m3.len() * 8 {
        let mut station = supplicant();
        station.receive(&hex(M1), true, snonce()).unwrap();
        let mut changed = m3.clone();
        changed[bit / 8] ^= 1 << (bit % 8);
        // Clearing the MIC bit makes it an (unauthenticated) message 1 with
        // the same ANonce, which is answered; every other flip is refused.
        // Key information is big-endian at bytes 5 and 6; MIC is 0x0100.
        let as_message_1 = changed[5] & 1 == 0 && m3[5] & 1 == 1;
        assert_eq!(
            station.receive(&changed, true, [0xee; 32]).is_ok(),
            as_message_1,
            "bit {bit}"
        );
        // Either way the genuine frame still completes the handshake.
        assert!(station.receive(&m3, true, snonce()).is_ok(), "bit {bit}");
    }
}

#[test]
fn a_retransmitted_message_1_keeps_the_pending_snonce() {
    let mut station = supplicant();
    station.receive(&hex(M1), true, snonce()).unwrap();
    let mut again = hex(M1);
    again[16] = 2;
    let actions = station.receive(&again, true, [0xee; 32]).unwrap();
    let Action::Send { frame, .. } = &actions[0] else {
        panic!("{actions:?}");
    };
    assert_eq!(parse_key(frame).unwrap().nonce, snonce());
    assert!(station.receive(&hex(M3), true, [0xee; 32]).is_ok());
    // A message 1 with a new ANonce starts over with the fresh SNonce.
    let mut station = supplicant();
    station.receive(&hex(M1), true, snonce()).unwrap();
    let mut other = hex(M1);
    other[17] ^= 0xff;
    let actions = station.receive(&other, true, [0xee; 32]).unwrap();
    let Action::Send { frame, .. } = &actions[0] else {
        panic!("{actions:?}");
    };
    assert_eq!(parse_key(frame).unwrap().nonce, [0xee; 32]);
}

#[test]
fn group_message_needs_the_pairwise_handshake_and_a_valid_mic() {
    let mut station = supplicant();
    assert!(station.receive(&hex(G1), true, snonce()).is_err());
    station.receive(&hex(M1), true, snonce()).unwrap();
    assert!(station.receive(&hex(G1), true, snonce()).is_err());
    station.receive(&hex(M3), true, snonce()).unwrap();
    let mut g1 = hex(G1);
    let last = g1.len() - 1;
    g1[last] ^= 1;
    assert!(station.receive(&g1, true, snonce()).is_err());
    assert!(station.receive(&hex(G1), true, snonce()).is_ok());
}

#[test]
fn message_3_must_repeat_the_beacons_rsn_element() {
    // The same handshake against a beacon claiming a different pairwise
    // suite list: message 3's element no longer matches.
    let mut beacon = ap_rsn();
    beacon.pairwise = vec![CIPHER_CCMP, 0x000f_ac02];
    let mut station = Supplicant::new(array(&hex(PMK)), AA, SPA, beacon);
    station.receive(&hex(M1), true, snonce()).unwrap();
    let error = station.receive(&hex(M3), true, snonce()).unwrap_err();
    assert!(error.contains("downgrade"), "{error}");
}

#[test]
fn message_3_without_message_1_or_with_another_anonce_is_refused() {
    let mut station = supplicant();
    assert!(station.receive(&hex(M3), true, snonce()).is_err());
    let mut m1 = hex(M1);
    m1[17] ^= 1;
    station.receive(&m1, true, snonce()).unwrap();
    assert!(station.receive(&hex(M3), true, snonce()).is_err());
}

/// A real authenticator drops message 2 under the wrong PMK and never sends
/// message 3; the station reports that through `awaiting_message_3`. One
/// that did send it would fail the MIC.
#[test]
fn a_wrong_passphrase_awaits_message_3_or_fails_its_mic() {
    let mut station = Supplicant::new([7; 32], AA, SPA, ap_rsn());
    assert!(!station.awaiting_message_3());
    station.receive(&hex(M1), true, snonce()).unwrap();
    assert!(station.awaiting_message_3());
    assert_eq!(
        station.receive(&hex(M3), true, snonce()).unwrap_err(),
        "message 3 MIC mismatch"
    );
}

/// `frame` with a new replay counter and its MIC recomputed under the PTK's
/// KCK, as the authenticator would retransmit it.
fn retransmitted(frame: &str, replay: u64) -> Vec<u8> {
    let mut frame = hex(frame);
    frame[9..17].copy_from_slice(&replay.to_be_bytes());
    frame[81..97].fill(0);
    let kck: [u8; 16] = array(&hex(PTK)[..16]);
    let mic = crate::sha1::hmac_sha1(&kck, &[&frame]);
    frame[81..97].copy_from_slice(&mic[..16]);
    frame
}

#[test]
fn a_repeated_message_3_gets_message_4_again_and_installs_nothing() {
    let mut station = supplicant();
    station.receive(&hex(M1), true, snonce()).unwrap();
    station.receive(&hex(M3), true, snonce()).unwrap();
    // Message 4 was lost; the authenticator, without the PTK yet, repeats
    // message 3 in the clear.
    let m4 = retransmitted(M4, 3);
    assert_eq!(
        station
            .receive(&retransmitted(M3, 3), false, snonce())
            .unwrap(),
        vec![Action::Send {
            frame: m4,
            encrypt: false
        }]
    );
    // Its replay counter is spent, and a forged MIC is still refused.
    assert!(station
        .receive(&retransmitted(M3, 3), true, snonce())
        .is_err());
    let mut forged = retransmitted(M3, 4);
    forged[90] ^= 1;
    assert_eq!(
        station.receive(&forged, true, snonce()).unwrap_err(),
        "message 3 MIC mismatch"
    );
}

#[test]
fn a_rekey_runs_encrypted_and_repeats_its_message_3_only_encrypted() {
    let mut station = supplicant();
    station.receive(&hex(M1), false, snonce()).unwrap();
    station.receive(&hex(M3), false, snonce()).unwrap();
    // The authenticator rekeys the PTK under the installed one.
    let mut m1 = hex(M1);
    m1[9..17].copy_from_slice(&3u64.to_be_bytes());
    station.receive(&m1, true, snonce()).unwrap();
    let plain = "unencrypted key message after the pairwise key";
    assert_eq!(
        station
            .receive(&retransmitted(M3, 4), false, snonce())
            .unwrap_err(),
        plain
    );
    let actions = station
        .receive(&retransmitted(M3, 4), true, snonce())
        .unwrap();
    assert!(actions.contains(&Action::Send {
        frame: retransmitted(M4, 4),
        encrypt: true
    }));
    assert!(actions
        .iter()
        .any(|action| matches!(action, Action::PairwiseKey { .. })));
    assert!(!actions.contains(&Action::Authorize));
    // Its message 3 repeats under the PTK, never in the clear.
    assert_eq!(
        station
            .receive(&retransmitted(M3, 5), false, snonce())
            .unwrap_err(),
        plain
    );
    assert_eq!(
        station
            .receive(&retransmitted(M3, 5), true, snonce())
            .unwrap(),
        vec![Action::Send {
            frame: retransmitted(M4, 5),
            encrypt: true
        }]
    );
}

#[test]
fn after_the_pairwise_key_only_the_first_message_3_may_repeat_in_the_clear() {
    let mut station = supplicant();
    station.receive(&hex(M1), false, snonce()).unwrap();
    station.receive(&hex(M3), false, snonce()).unwrap();
    for frame in [retransmitted(M1, 3), retransmitted(G1, 3)] {
        assert_eq!(
            station.receive(&frame, false, snonce()).unwrap_err(),
            "unencrypted key message after the pairwise key"
        );
    }
    assert!(station
        .receive(&retransmitted(G1, 3), true, snonce())
        .is_ok());
}

#[test]
fn an_identical_gtk_is_acknowledged_but_not_installed_again() {
    let mut station = supplicant();
    station.receive(&hex(M1), true, snonce()).unwrap();
    station.receive(&hex(M3), true, snonce()).unwrap();
    station.receive(&hex(G1), true, snonce()).unwrap();
    let actions = station
        .receive(&retransmitted(G1, 4), true, snonce())
        .unwrap();
    assert!(
        matches!(actions.as_slice(), [Action::Send { encrypt: true, .. }]),
        "{actions:?}"
    );
}

#[test]
fn padding_after_the_eapol_body_is_not_part_of_the_mic() {
    let mut station = supplicant();
    let mut m1 = hex(M1);
    m1.extend_from_slice(&[0; 10]);
    station.receive(&m1, true, snonce()).unwrap();
    let mut m3 = hex(M3);
    m3.extend_from_slice(&[0; 10]);
    assert_eq!(station.receive(&m3, true, snonce()).unwrap().len(), 4);
    assert!(!station.awaiting_message_3());
}

#[test]
fn truncated_frames_are_refused_without_panicking() {
    for frame in [M1, M3, G1] {
        let frame = hex(frame);
        for cut in 0..frame.len() {
            let mut station = supplicant();
            let _ = station.receive(&frame[..cut], true, snonce());
        }
    }
}

#[test]
fn rsn_parsing_and_security_classification() {
    let rsn = ap_rsn();
    assert_eq!(rsn.group, CIPHER_CCMP);
    assert_eq!(rsn.pairwise, vec![CIPHER_CCMP]);
    assert_eq!(rsn.akm, vec![AKM_PSK]);
    assert_eq!(rsn.capabilities, 0x000c);
    assert_eq!(parse_rsn(&STATION_RSN[2..]).unwrap().capabilities, 0);
    let with = |rsn: &[u8]| {
        let mut ies = vec![0, 3, b'a', b'b', b'c'];
        ies.extend_from_slice(rsn);
        security(&ies)
    };
    assert!(matches!(with(&hex(AP_RSN)), Security::Wpa2Psk(_)));
    assert_eq!(with(&[]), Security::Open);
    // WPA3-SAE only (AKM 8).
    let sae = hex("30140100000fac040100000fac040100000fac08c000");
    assert!(matches!(with(&sae), Security::Unsupported(_)));
    // WPA2/WPA3 transition: PSK and SAE, MFP capable but not required.
    let mixed = hex("30180100000fac040100000fac040200000fac02000fac088000");
    assert!(matches!(with(&mixed), Security::Wpa2Psk(_)));
    // MFP required.
    let mfpr = hex("30140100000fac040100000fac040100000fac02c000");
    assert!(matches!(with(&mfpr), Security::Unsupported(_)));
    // TKIP group.
    let tkip = hex("30140100000fac020100000fac040100000fac020000");
    assert!(matches!(with(&tkip), Security::Unsupported(_)));
    // WPA1 vendor element only.
    assert!(matches!(
        with(&hex("dd160050f20101000050f20201000050f20201000050f202")),
        Security::Unsupported("WPA1 only")
    ));
    for cut in 0..AP_RSN.len() / 2 {
        let _ = parse_rsn(&hex(AP_RSN)[2..2 + cut.saturating_sub(2)]);
    }
}

#[test]
fn choose_takes_the_strongest_matching_bss() {
    let ies = |name: &[u8], rsn: &str| {
        let mut ies = vec![0, name.len() as u8];
        ies.extend_from_slice(name);
        ies.extend(hex(rsn));
        ies
    };
    let bss = |last: u8, signal: i32, ies: Vec<u8>| Bss {
        bssid: [2, 0, 0, 0, 0, last],
        frequency: 2412,
        signal_mbm: signal,
        ies,
    };
    let list = vec![
        bss(1, -7000, ies(b"home", AP_RSN)),
        bss(2, -4000, ies(b"home", AP_RSN)),
        bss(3, -3000, ies(b"other", AP_RSN)),
        bss(4, -2000, ies(b"home", "")),
    ];
    assert_eq!(choose(&list, b"home", true).unwrap().bssid[5], 2);
    assert_eq!(choose(&list, b"home", false).unwrap().bssid[5], 4);
    assert_eq!(
        choose(&list, b"absent", true).unwrap_err(),
        "network not found"
    );
    assert!(choose(&list[3..], b"home", true)
        .unwrap_err()
        .contains("--open"));
}

#[test]
fn elements_stop_at_a_truncated_element() {
    let ies = [0, 2, b'h', b'i', 48, 200, 1];
    let found: Vec<_> = elements(&ies).collect();
    assert_eq!(found, vec![(0, &b"hi"[..])]);
    assert_eq!(ssid(&ies), Some(&b"hi"[..]));
}
