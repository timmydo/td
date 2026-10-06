#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use td_install::installation_plan::{
    Basis, Candidates, Destination, DestinationObservation, Plan, Settings, Zones, MAX_CANDIDATES,
    MAX_CANDIDATE_BYTES, MAX_ZONES,
};
use td_install::installation_protocol::{
    check_greeting, frame, payload_len, scrub, Abandon, Ending, Failure, Phase, RecoveryDigits,
    Refusal, Reply, Request, ReviewNonce, State, GREETING, MAX_REPLY_BYTES, MAX_REQUEST_BYTES,
    RECOVERY_DIGITS,
};

// Independently specified: the destination and plan bytes of
// tests/installation_plan.rs.
const DESTINATION: &[u8] =
    b"\0\0\x01\x03\0\0\0\x03\0\0\0\0\0\0\0\x1b\0\0\0\x01\x80\0\0\0\0\0\x10\0\0\
\0\x07nvme0n1\x01\0\x07Model A\x01\0\x09serial-42\0";
const SETTINGS: &[u8] = b"\0\x05alice\0\x09td-laptop\0\x02us\0\x07Etc/UTC";

fn destination() -> Destination {
    Destination::new(DestinationObservation {
        name: "nvme0n1",
        major: 259,
        minor: 3,
        sequence: 27,
        capacity: 6 << 30,
        sector: 4096,
        removable: false,
        model: Some("Model A"),
        serial: Some("serial-42"),
        wwid: None,
    })
    .unwrap()
}
fn settings() -> Settings {
    Settings::new("alice", "td-laptop", "us", "Etc/UTC").unwrap()
}
fn uuid() -> [u8; 16] {
    [
        0x19, 2, 3, 4, 5, 6, 0x47, 8, 0x89, 10, 11, 12, 13, 14, 15, 16,
    ]
}
fn plan() -> Plan {
    Plan::new(
        [1; 32],
        destination(),
        [2; 32],
        uuid(),
        Basis::new(true, false),
        settings(),
    )
    .unwrap()
}
fn plan_bytes() -> Vec<u8> {
    let mut bytes = b"TDPLAN03".to_vec();
    bytes.extend([1; 32]);
    bytes.extend([2; 32]);
    bytes.extend(uuid());
    // The storage and basis bytes follow the destination's removable flag:
    // unencrypted, with a usable TPM and no keyboard console.
    let (fixed, names) = DESTINATION.split_at(29);
    bytes.extend(fixed);
    bytes.extend([0, 1]);
    bytes.extend(names);
    bytes.extend(SETTINGS);
    bytes
}
/// Forty-eight digits: eight groups of five and a check digit.
const KEY: &[u8; RECOVERY_DIGITS] = b"012345678901234567890123456789012345678901234567";
fn digits() -> RecoveryDigits {
    RecoveryDigits::new(KEY).unwrap()
}
fn nonce() -> ReviewNonce {
    ReviewNonce::new([7; 32]).unwrap()
}
fn zones() -> Zones {
    Zones::new(vec!["America/Los_Angeles".into(), "Etc/UTC".into()]).unwrap()
}

fn requests() -> Vec<Request> {
    vec![
        Request::Destinations,
        Request::Propose {
            destination: destination(),
            settings: settings(),
        },
        Request::Execute(plan()),
        Request::Status,
        Request::Withdraw(nonce()),
        Request::Timezones,
        Request::End(Ending::Restart, nonce()),
        Request::End(Ending::PowerOff, nonce()),
        Request::RecoveryKey(nonce()),
        Request::ConfirmRecovery(nonce(), digits()),
    ]
}

fn states() -> Vec<State> {
    let mut states = vec![
        State::Idle,
        State::Reviewed(nonce()),
        State::AwaitingConsent(nonce()),
        State::Complete(nonce()),
    ];
    states.extend(Phase::ALL.iter().map(|&p| State::Running(nonce(), p)));
    states.extend(Failure::ALL.iter().map(|&f| State::Failed(nonce(), f)));
    states.extend(Abandon::ALL.iter().map(|&a| State::Abandoned(nonce(), a)));
    states
}

fn replies() -> Vec<Reply> {
    let mut replies = vec![
        Reply::Destinations(Candidates::new(vec![destination()]).unwrap()),
        Reply::Reviewed(Box::new(plan())),
    ];
    replies.extend(states().into_iter().map(Reply::Status));
    replies.extend(Refusal::ALL.iter().map(|&r| Reply::Refused(r)));
    replies.push(Reply::Timezones(zones()));
    replies.push(Reply::RecoveryKey(nonce(), digits()));
    replies
}

#[test]
fn wire_bytes_are_independently_specified() {
    assert_eq!(GREETING, b"TDINS06\n");
    let mut propose = vec![0x02];
    propose.extend(DESTINATION);
    propose.extend(SETTINGS);
    let mut execute = vec![0x03];
    execute.extend(plan_bytes());
    let mut withdraw = vec![0x05];
    withdraw.extend([7; 32]);
    let mut restart = vec![0x07];
    restart.extend([7; 32]);
    let mut power_off = vec![0x08];
    power_off.extend([7; 32]);
    let mut recovery = vec![0x09];
    recovery.extend([7; 32]);
    let mut confirm = vec![0x0a];
    confirm.extend([7; 32]);
    confirm.extend(KEY);
    let expected: [&[u8]; 10] = [
        &[0x01],
        &propose,
        &execute,
        &[0x04],
        &withdraw,
        &[0x06],
        &restart,
        &power_off,
        &recovery,
        &confirm,
    ];
    for (request, bytes) in requests().iter().zip(expected) {
        assert_eq!(request.encode(), bytes, "{request:?}");
        assert_eq!(&Request::decode(bytes).unwrap(), request);
    }

    let mut destinations = b"\x81TDCAND01\x01".to_vec();
    destinations.extend(DESTINATION);
    let mut reviewed = vec![0x82];
    reviewed.extend(plan_bytes());
    let status = |code: u8, detail: Option<u8>| {
        let mut bytes = vec![0x83, code];
        bytes.extend([7; 32]);
        bytes.extend(detail);
        bytes
    };
    for (reply, bytes) in [
        (
            Reply::Destinations(Candidates::new(vec![destination()]).unwrap()),
            destinations,
        ),
        (Reply::Reviewed(Box::new(plan())), reviewed),
        (Reply::Status(State::Idle), vec![0x83, 0]),
        (Reply::Status(State::Reviewed(nonce())), status(1, None)),
        (
            Reply::Status(State::AwaitingConsent(nonce())),
            status(2, None),
        ),
        (
            Reply::Status(State::Running(nonce(), Phase::PreparingDisk)),
            status(3, Some(1)),
        ),
        (
            Reply::Status(State::Running(nonce(), Phase::VerifyingBoot)),
            status(3, Some(5)),
        ),
        (
            Reply::Status(State::Running(nonce(), Phase::RecoveryKey)),
            status(3, Some(6)),
        ),
        (Reply::Status(State::Complete(nonce())), status(4, None)),
        (
            Reply::Status(State::Failed(nonce(), Failure::DestinationChanged)),
            status(5, Some(1)),
        ),
        (
            Reply::Status(State::Failed(nonce(), Failure::SettingsFailed)),
            status(5, Some(5)),
        ),
        (
            Reply::Status(State::Failed(nonce(), Failure::RecoveryUnconfirmed)),
            status(5, Some(6)),
        ),
        (
            Reply::Status(State::Abandoned(nonce(), Abandon::Withdrawn)),
            status(6, Some(1)),
        ),
        (
            Reply::Status(State::Abandoned(nonce(), Abandon::ConsentExpired)),
            status(6, Some(3)),
        ),
        (
            Reply::Status(State::Abandoned(nonce(), Abandon::DestinationChanged)),
            status(6, Some(5)),
        ),
        (Reply::Refused(Refusal::Busy), vec![0x84, 1]),
        (Reply::Refused(Refusal::NoReview), vec![0x84, 12]),
        (Reply::Refused(Refusal::ConsentUnavailable), vec![0x84, 13]),
        (
            Reply::Refused(Refusal::TimezonesUnavailable),
            vec![0x84, 14],
        ),
        (Reply::Refused(Refusal::PowerUnavailable), vec![0x84, 15]),
        (Reply::Refused(Refusal::RecoveryKeySent), vec![0x84, 16]),
        (Reply::Refused(Refusal::RecoveryKeyMismatch), vec![0x84, 17]),
        (
            Reply::RecoveryKey(nonce(), digits()),
            [&[0x86], &[7; 32][..], KEY].concat(),
        ),
        (
            Reply::Timezones(zones()),
            b"\x85TDZONE01\0\x02\0\x13America/Los_Angeles\0\x07Etc/UTC".to_vec(),
        ),
    ] {
        assert_eq!(reply.encode(), bytes, "{reply:?}");
        assert_eq!(Reply::decode(&bytes).unwrap(), reply);
    }
}

#[test]
fn every_message_round_trips_and_codes_are_dense() {
    for request in requests() {
        assert_eq!(Request::decode(&request.encode()).unwrap(), request);
    }
    for reply in replies() {
        assert_eq!(Reply::decode(&reply.encode()).unwrap(), reply);
    }
    let codes = |replies: Vec<Reply>| -> Vec<u8> {
        replies
            .iter()
            .map(|reply| *reply.encode().last().unwrap())
            .collect()
    };
    let expect = |count: u8| (1..=count).collect::<Vec<_>>();
    assert_eq!(
        codes(Refusal::ALL.iter().map(|&r| Reply::Refused(r)).collect()),
        expect(17)
    );
    let status = |state| Reply::Status(state);
    assert_eq!(
        codes(
            Phase::ALL
                .iter()
                .map(|&p| status(State::Running(nonce(), p)))
                .collect()
        ),
        expect(6)
    );
    assert_eq!(
        codes(
            Failure::ALL
                .iter()
                .map(|&f| status(State::Failed(nonce(), f)))
                .collect()
        ),
        expect(6)
    );
    assert_eq!(
        codes(
            Abandon::ALL
                .iter()
                .map(|&a| status(State::Abandoned(nonce(), a)))
                .collect()
        ),
        expect(5)
    );
    assert_eq!(State::Idle.review(), None);
    for state in states().into_iter().skip(1) {
        assert_eq!(state.review(), Some(nonce()));
    }
    assert_eq!(ReviewNonce::from(&plan()).as_bytes(), &[1; 32]);
}

#[test]
fn maximal_messages_fit_their_direction_bounds() {
    let label = "X".repeat(256);
    let maximal = |index: usize| {
        Destination::new(DestinationObservation {
            name: &format!("{:0<64}", format!("vd{index:02}")),
            major: 253,
            minor: index as u32,
            sequence: index as u64 + 1,
            capacity: 6 << 30,
            sector: 4096,
            removable: false,
            model: Some(&label),
            serial: Some(&label),
            wwid: Some(&label),
        })
        .unwrap()
    };
    let disks = (0..MAX_CANDIDATES).map(maximal).collect::<Vec<_>>();
    let reply = Reply::Destinations(Candidates::new(disks).unwrap());
    assert_eq!(reply.encode().len(), 1 + MAX_CANDIDATE_BYTES);
    assert_eq!(Reply::decode(&reply.encode()).unwrap(), reply);
    // The widest catalog is the widest reply.
    let ids = (0..MAX_ZONES)
        .map(|index| format!("Zone/{index:059}"))
        .collect::<Vec<_>>();
    let reply = Reply::Timezones(Zones::new(ids).unwrap());
    assert_eq!(reply.encode().len(), MAX_REPLY_BYTES);
    assert_eq!(MAX_REPLY_BYTES, 67_595);
    assert_eq!(Reply::decode(&reply.encode()).unwrap(), reply);

    let settings = Settings::new(
        &"a".repeat(32),
        &"h".repeat(63),
        &"k".repeat(64),
        &"z".repeat(64),
    )
    .unwrap();
    let propose = Request::Propose {
        destination: maximal(0),
        settings: settings.clone(),
    };
    // Propose carries no storage byte: the service chooses storage.
    assert_eq!(propose.encode().len(), 1104);
    assert_eq!(Request::decode(&propose.encode()).unwrap(), propose);
    let plan = Plan::new(
        [255; 32],
        maximal(0),
        [0; 32],
        uuid(),
        Basis::new(true, true),
        settings,
    )
    .unwrap();
    let execute = Request::Execute(plan.clone());
    assert_eq!(execute.encode().len(), 1194);
    assert!(execute.encode().len() <= MAX_REQUEST_BYTES);
    assert_eq!(Request::decode(&execute.encode()).unwrap(), execute);
    let reviewed = Reply::Reviewed(Box::new(plan));
    assert_eq!(reviewed.encode().len(), 1194);
    assert_eq!(Reply::decode(&reviewed.encode()).unwrap(), reviewed);
    let status = Reply::Status(State::Running(nonce(), Phase::VerifyingBoot));
    assert_eq!(status.encode().len(), 35);
    let key = Reply::RecoveryKey(nonce(), digits());
    assert_eq!(key.encode().len(), 1 + 32 + 48);
    let confirm = Request::ConfirmRecovery(nonce(), digits());
    assert_eq!(confirm.encode().len(), 1 + 32 + 48);
}

#[test]
fn recovery_digits_are_exactly_48_ascii_digits_and_never_printed() {
    assert_eq!(digits().as_bytes(), KEY);
    for bad in [
        &KEY[..47],
        &[KEY.as_slice(), b"0"].concat(),
        b"01234567890123456789012345678901234567890123456a",
        b"01234-678901234567890123456789012345678901234567",
        b"0123 5678901234567890123456789012345678901234567",
    ] {
        assert_eq!(
            RecoveryDigits::new(bad).unwrap_err(),
            "installation recovery key is not 48 digits"
        );
    }
    // A message carrying digits other than ASCII decimals refuses.
    let mut reply = Reply::RecoveryKey(nonce(), digits()).encode();
    *reply.last_mut().unwrap() = b'-';
    assert!(Reply::decode(&reply).is_err());
    let mut confirm = Request::ConfirmRecovery(nonce(), digits()).encode();
    confirm[40] = b' ';
    assert!(Request::decode(&confirm).is_err());
    for printed in [
        format!("{:?}", digits()),
        format!("{:?}", Reply::RecoveryKey(nonce(), digits())),
        format!("{:?}", Request::ConfirmRecovery(nonce(), digits())),
    ] {
        assert!(!printed.contains("0123"), "{printed}");
    }
}

#[test]
fn recovery_digits_compare_every_byte_and_payloads_scrub() {
    assert_eq!(digits(), digits());
    for index in [0, 23, 47] {
        let mut other = *KEY;
        other[index] = if other[index] == b'9' { b'0' } else { b'9' };
        assert_ne!(RecoveryDigits::new(&other).unwrap(), digits());
    }
    let mut payload = Request::ConfirmRecovery(nonce(), digits()).encode();
    scrub(&mut payload);
    assert_eq!(payload, vec![0; 1 + 32 + 48]);
}

#[test]
fn oversize_refuses_before_parsing() {
    let mut request = vec![0x01];
    request.resize(MAX_REQUEST_BYTES + 1, 0);
    assert_eq!(
        Request::decode(&request).unwrap_err(),
        "installation request exceeds wire bound"
    );
    let mut reply = vec![0x83, 0];
    reply.resize(MAX_REPLY_BYTES + 1, 0);
    assert_eq!(
        Reply::decode(&reply).unwrap_err(),
        "installation reply exceeds wire bound"
    );
}

#[test]
fn every_truncation_and_extension_refuses() {
    let encoded = requests()
        .iter()
        .map(|r| (r.encode(), true))
        .chain(replies().iter().map(|r| (r.encode(), false)))
        .collect::<Vec<_>>();
    for (bytes, request) in encoded {
        let decodes = |candidate: &[u8]| {
            if request {
                Request::decode(candidate).is_ok()
            } else {
                Reply::decode(candidate).is_ok()
            }
        };
        for end in 0..bytes.len() {
            assert!(!decodes(&bytes[..end]), "{bytes:?} at {end}");
        }
        for extra in [vec![0], vec![b'\n'], bytes.clone()] {
            let mut extended = bytes.clone();
            extended.extend(extra);
            assert!(!decodes(&extended), "{bytes:?} extended");
        }
    }
}

#[test]
fn unknown_tags_codes_and_zero_nonces_refuse() {
    for tag in 0..=255u8 {
        let known_request = (0x01..=0x0a).contains(&tag);
        let known_reply = (0x81..=0x86).contains(&tag);
        if !known_request {
            assert_eq!(
                Request::decode(&[tag]).unwrap_err(),
                "unknown installation request",
                "{tag}"
            );
        }
        if !known_reply {
            assert_eq!(
                Reply::decode(&[tag, 0]).unwrap_err(),
                "unknown installation reply",
                "{tag}"
            );
        }
    }
    let status = |code: u8, detail: Option<u8>| {
        let mut bytes = vec![0x83, code];
        bytes.extend([7; 32]);
        bytes.extend(detail);
        Reply::decode(&bytes)
    };
    for code in 7..=255 {
        assert_eq!(
            status(code, None).unwrap_err(),
            "unknown installation state"
        );
        // The code is judged before any nonce is read.
        assert_eq!(
            Reply::decode(&[0x83, code]).unwrap_err(),
            "unknown installation state"
        );
    }
    for (state, limit, what) in [(3, 6, "phase"), (5, 6, "failure"), (6, 5, "abandonment")] {
        for detail in (0..=255).filter(|d| *d == 0 || *d > limit) {
            assert_eq!(
                status(state, Some(detail)).unwrap_err(),
                format!("unknown installation {what}")
            );
        }
    }
    for code in (0..=255).filter(|c| *c == 0 || *c > 17) {
        assert_eq!(
            Reply::decode(&[0x84, code]).unwrap_err(),
            "unknown installation refusal"
        );
    }
    let mut zero_status = vec![0x83, 1];
    zero_status.extend([0; 32]);
    let mut zero_withdraw = vec![0x05];
    zero_withdraw.extend([0; 32]);
    let mut zero_restart = vec![0x07];
    zero_restart.extend([0; 32]);
    let mut zero_power_off = vec![0x08];
    zero_power_off.extend([0; 32]);
    let mut zero_recovery = vec![0x09];
    zero_recovery.extend([0; 32]);
    let mut zero_confirm = vec![0x0a];
    zero_confirm.extend([0; 32]);
    zero_confirm.extend(KEY);
    let mut zero_key = vec![0x86];
    zero_key.extend([0; 32]);
    zero_key.extend(KEY);
    for error in [
        Reply::decode(&zero_status).unwrap_err(),
        Request::decode(&zero_withdraw).unwrap_err(),
        Request::decode(&zero_restart).unwrap_err(),
        Request::decode(&zero_power_off).unwrap_err(),
        Request::decode(&zero_recovery).unwrap_err(),
        Request::decode(&zero_confirm).unwrap_err(),
        Reply::decode(&zero_key).unwrap_err(),
    ] {
        assert_eq!(error, "installation review nonce cannot be zero");
    }
    assert!(ReviewNonce::new([0; 32]).is_err());
}

#[test]
fn directions_are_disjoint() {
    for request in requests() {
        assert!(Reply::decode(&request.encode()).is_err(), "{request:?}");
    }
    for reply in replies() {
        assert!(Request::decode(&reply.encode()).is_err(), "{reply:?}");
    }
}

#[test]
fn only_this_version_is_admitted() {
    assert!(check_greeting(b"TDINS06\n").is_ok());
    for other in [
        b"TDINS05\n",
        b"TDINS04\n",
        b"TDINS03\n",
        b"TDINS02\n",
        b"TDAT001\n",
        b"TDUPD01\n",
        b"TDINS06\0",
    ] {
        assert_eq!(
            check_greeting(other).unwrap_err(),
            "unsupported installation protocol greeting"
        );
    }
    let mut execute = Request::Execute(plan()).encode();
    execute[8] = b'1';
    assert!(Request::decode(&execute).is_err());
    let mut reviewed = Reply::Reviewed(Box::new(plan())).encode();
    reviewed[8] = b'1';
    assert!(Reply::decode(&reviewed).is_err());
    let mut destinations = Reply::Destinations(Candidates::new(vec![]).unwrap()).encode();
    destinations[8] = b'2';
    assert!(Reply::decode(&destinations).is_err());
    let mut timezones = Reply::Timezones(zones()).encode();
    timezones[8] = b'2';
    assert!(Reply::decode(&timezones).is_err());
}

#[test]
fn messages_are_canonical_under_every_single_byte_change() {
    let status = Reply::Status(State::Failed(nonce(), Failure::WriteFailed));
    let cases: Vec<(Vec<u8>, bool)> = vec![
        (requests()[1].encode(), true),
        (requests()[2].encode(), true),
        (requests()[4].encode(), true),
        (requests()[9].encode(), true),
        (status.encode(), false),
    ];
    for (bytes, request) in cases {
        let mut decoded = 0;
        for offset in 0..bytes.len() {
            for replacement in 0..=255 {
                let mut changed = bytes.clone();
                changed[offset] = replacement;
                let reencoded = if request {
                    Request::decode(&changed).map(|m| m.encode())
                } else {
                    Reply::decode(&changed).map(|m| m.encode())
                };
                if let Ok(reencoded) = reencoded {
                    assert_eq!(reencoded, changed, "offset {offset}");
                    decoded += 1;
                }
            }
        }
        // Every byte's own value decodes, and so do other label bytes.
        assert!(decoded > bytes.len(), "{decoded}");
    }
}

#[test]
fn propose_reuses_field_admission() {
    let mut bytes = vec![0x02];
    bytes.extend(DESTINATION);
    bytes.extend(SETTINGS);
    let name = 1 + 29;
    let mut bad_name = bytes.clone();
    bad_name[name + 2] = b'/';
    assert_eq!(
        Request::decode(&bad_name).unwrap_err(),
        "invalid installation destination kernel name"
    );
    let mut bad_sector = bytes.clone();
    bad_sector[1 + 24..1 + 28].copy_from_slice(&1024u32.to_be_bytes());
    assert!(Request::decode(&bad_sector).is_err());
    let settings_at = 1 + DESTINATION.len();
    let mut spaced = bytes.clone();
    spaced[settings_at + 3] = b' ';
    assert_eq!(
        Request::decode(&spaced).unwrap_err(),
        "plan choice cannot contain spaces"
    );
    let mut long = bytes[..settings_at].to_vec();
    long.extend((33u16).to_be_bytes());
    long.extend([b'a'; 33]);
    long.extend(&SETTINGS[7..]);
    assert_eq!(
        Request::decode(&long).unwrap_err(),
        "invalid request string length"
    );
}

#[test]
fn framing_admits_only_nonzero_lengths_within_each_bound() {
    assert_eq!(frame(b"\x04", 1).unwrap(), b"\0\0\0\x01\x04");
    assert!(frame(b"", 1).is_err());
    assert!(frame(b"\x04\x04", 1).is_err());
    let reply = vec![0x81; MAX_REPLY_BYTES];
    assert_eq!(
        frame(&reply, MAX_REPLY_BYTES).unwrap().len(),
        4 + MAX_REPLY_BYTES
    );
    assert!(frame(&[reply.as_slice(), &[0]].concat(), MAX_REPLY_BYTES).is_err());
    assert!(frame(&reply, MAX_REQUEST_BYTES).is_err());
    for (limit, at, over) in [
        (MAX_REQUEST_BYTES, [0, 0, 8, 1], [0, 0, 8, 2]),
        (MAX_REPLY_BYTES, [0, 1, 0x08, 0x0b], [0, 1, 0x08, 0x0c]),
    ] {
        assert_eq!(payload_len(at, limit).unwrap(), limit);
        for header in [[0, 0, 0, 0], over, [255, 255, 255, 255]] {
            assert_eq!(
                payload_len(header, limit).unwrap_err(),
                "installation frame outside its bound"
            );
        }
    }
}

#[test]
fn replies_pair_only_with_their_requests() {
    let expected = |request: &Request, reply: &Reply| match (request, reply) {
        (Request::Status, Reply::Status(_)) => true,
        (Request::Status, _) => false,
        (_, Reply::Refused(_)) => true,
        (Request::Destinations, Reply::Destinations(_)) => true,
        (Request::Propose { .. }, Reply::Reviewed(_)) => true,
        (
            Request::Execute(_)
            | Request::Withdraw(_)
            | Request::End(..)
            | Request::ConfirmRecovery(..),
            Reply::Status(_),
        ) => true,
        (Request::Timezones, Reply::Timezones(_)) => true,
        (Request::RecoveryKey(_), Reply::RecoveryKey(..)) => true,
        _ => false,
    };
    for request in requests() {
        for reply in replies() {
            assert_eq!(
                reply.answers(&request),
                expected(&request, &reply),
                "{request:?} {reply:?}"
            );
        }
    }
}
