//! Prediction tests over logs OVMF recorded under swtpm
//! (`tests/eventlog/README`), and refusals built from them.
use super::*;

const A: &[u8] = include_bytes!("../tests/eventlog/ovmf-a.log");
const B: &[u8] = include_bytes!("../tests/eventlog/ovmf-b.log");
const RECORD: &str = include_str!("../tests/eventlog/ovmf.txt");

fn hex(text: &str) -> [u8; 32] {
    assert_eq!(text.len(), 64, "{text}");
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(bytes);
    hash.finalize()
}

fn recorded(chain: &str, key: &str) -> [u8; 32] {
    let lines: Vec<&str> = RECORD
        .lines()
        .filter_map(|line| line.strip_prefix(&format!("{chain} {key} ")))
        .collect();
    assert_eq!(lines.len(), 1, "{chain} {key}");
    hex(lines[0])
}

fn read(chain: &str) -> Pcrs {
    Pcrs {
        pcr4: recorded(chain, "pcr4"),
        pcr7: recorded(chain, "pcr7"),
        pcr9: recorded(chain, "pcr9"),
    }
}

fn chain(name: &str) -> Chain {
    Chain {
        application: recorded(name, "application"),
        initrd: recorded(name, "initrd"),
    }
}

/// An event to encode, owning its data.
#[derive(Clone, Debug, PartialEq)]
struct Edit {
    pcr: u32,
    kind: u32,
    sha256: [u8; 32],
    data: Vec<u8>,
}

fn edit(event: &Event<'_>) -> Edit {
    Edit {
        pcr: event.pcr,
        kind: event.kind,
        sha256: event.sha256,
        data: event.data.to_vec(),
    }
}

fn header(log: &[u8]) -> &[u8] {
    let size = u32::from_le_bytes(log[28..32].try_into().unwrap()) as usize;
    &log[..32 + size]
}

/// `log`'s Spec ID event followed by `events`, each with its SHA-256
/// digest and zeros for the log's other algorithms.
fn encode(log: &[u8], events: &[Edit]) -> Vec<u8> {
    let algorithms = parse(log).unwrap().algorithms;
    let mut out = header(log).to_vec();
    for event in events {
        out.extend_from_slice(&event.pcr.to_le_bytes());
        out.extend_from_slice(&event.kind.to_le_bytes());
        out.extend_from_slice(&(algorithms.len() as u32).to_le_bytes());
        for (id, size) in &algorithms {
            out.extend_from_slice(&id.to_le_bytes());
            if *id == TPM_ALG_SHA256 {
                out.extend_from_slice(&event.sha256);
            } else {
                out.extend_from_slice(&vec![0u8; *size]);
            }
        }
        out.extend_from_slice(&(event.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&event.data);
    }
    out
}

/// The PCRs `events` extend, replayed here apart from `predict`.
fn replay(events: &[Edit]) -> Pcrs {
    let mut pcrs = [[0u8; 32]; 3];
    for event in events.iter().filter(|e| e.kind != EV_NO_ACTION) {
        let slot = match event.pcr {
            4 => 0,
            7 => 1,
            9 => 2,
            _ => continue,
        };
        let mut hash = Sha256::new();
        hash.update(&pcrs[slot]);
        hash.update(&event.sha256);
        pcrs[slot] = hash.finalize();
    }
    Pcrs {
        pcr4: pcrs[0],
        pcr7: pcrs[1],
        pcr9: pcrs[2],
    }
}

fn events(log: &[u8]) -> Vec<Edit> {
    parse(log).unwrap().events.iter().map(edit).collect()
}

fn position(events: &[Edit], pcr: u32, kind: u32) -> usize {
    let found: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.pcr == pcr && e.kind == kind)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(found.len(), 1, "PCR {pcr} kind {kind:#x}");
    found[0]
}

/// Predict over chain a's log changed by `change`, against the PCRs the
/// changed log replays to, with chain a's digests.
fn changed(change: impl FnOnce(&mut Vec<Edit>)) -> Result<Pcrs, Refusal> {
    let mut list = events(A);
    change(&mut list);
    predict(&encode(A, &list), &replay(&list), &chain("a"), &chain("b"))
}

fn action(pcr: u32, text: &[u8]) -> Edit {
    Edit {
        pcr,
        kind: EV_EFI_ACTION,
        sha256: digest(text),
        data: text.to_vec(),
    }
}

#[test]
fn ovmf_logs_replay_to_the_pcrs_read() {
    for (name, log) in [("a", A), ("b", B)] {
        assert_eq!(
            predict(log, &read(name), &chain(name), &chain(name)),
            Ok(read(name)),
            "{name}"
        );
        assert_eq!(replay(&events(log)), read(name), "{name}");
    }
}

#[test]
fn the_substitution_predicts_the_other_chain_s_pcrs() {
    let (a, b) = (read("a"), read("b"));
    assert_ne!(a.pcr4, b.pcr4);
    assert_ne!(a.pcr9, b.pcr9);
    assert_eq!(a.pcr7, b.pcr7);
    assert_eq!(predict(A, &a, &chain("a"), &chain("b")), Ok(b));
    assert_eq!(predict(B, &b, &chain("b"), &chain("a")), Ok(a));
}

#[test]
fn the_logs_measure_the_recorded_chains() {
    for (name, log) in [("a", A), ("b", B)] {
        let list = events(log);
        let application = &list[position(&list, 4, EV_EFI_BOOT_SERVICES_APPLICATION)];
        assert_eq!(application.sha256, recorded(name, "application"), "{name}");
        // Authenticode, not the file's plain digest.
        assert_ne!(
            application.sha256,
            recorded(name, "kernel-sha256"),
            "{name}"
        );
        let initrd = &list[position(&list, 9, EV_EVENT_TAG)];
        assert_eq!(initrd.data, INITRD_EVENT, "{name}");
        assert_eq!(initrd.sha256, recorded(name, "initrd"), "{name}");
    }
}

/// td-boot's Authenticode digest of the kernel these logs were recorded
/// with is the one OVMF measured. The kernel is not committed: run with
/// TD_BOOT_RECORDED_KERNEL naming the bzImage `tests/eventlog/README`
/// names; chain b's kernel is it with 512 zero bytes appended.
#[test]
#[ignore = "needs the recorded kernel, named by TD_BOOT_RECORDED_KERNEL"]
fn authenticode_agrees_with_ovmf_over_the_recorded_kernel() {
    let path = std::env::var_os("TD_BOOT_RECORDED_KERNEL").expect("TD_BOOT_RECORDED_KERNEL");
    let kernel = std::fs::read(path).unwrap();
    let padded = [kernel.as_slice(), &[0u8; 512]].concat();
    for (name, image) in [("a", kernel), ("b", padded)] {
        assert_eq!(digest(&image), recorded(name, "kernel-sha256"), "{name}");
        assert_eq!(
            crate::authenticode::sha256(&image),
            Ok(recorded(name, "application")),
            "{name}"
        );
    }
}

#[test]
fn no_action_events_are_skipped() {
    let mut list = events(A);
    let before = replay(&list);
    for (at, pcr) in [(0, 4), (3, 9), (5, 7), (list.len(), 4)] {
        list.insert(
            at,
            Edit {
                pcr,
                kind: EV_NO_ACTION,
                sha256: [0x5a; 32],
                data: RETURNING.to_vec(),
            },
        );
    }
    assert_eq!(replay(&list), before);
    assert_eq!(
        predict(&encode(A, &list), &read("a"), &chain("a"), &chain("b")),
        Ok(read("b"))
    );
}

#[test]
fn the_final_events_table_is_replayed_with_the_log() {
    // OVMF logs ExitBootServices after the stub took the log, so Linux
    // presents those events from the final-events table, after the rest.
    let list = events(A);
    let tail: Vec<&[u8]> = list[list.len() - 2..]
        .iter()
        .map(|e| e.data.as_slice())
        .collect();
    assert_eq!(
        tail,
        [
            b"Exit Boot Services Invocation".as_slice(),
            b"Exit Boot Services Returned with Success".as_slice()
        ]
    );
    // What follows them counts as any other event does.
    let appended = action(7, b"td appended");
    let mut longer = list.clone();
    longer.push(appended.clone());
    let pcrs = replay(&longer);
    assert_ne!(pcrs.pcr7, read("a").pcr7);
    let log = encode(A, &longer);
    assert_eq!(predict(&log, &pcrs, &chain("a"), &chain("a")), Ok(pcrs));
    assert_eq!(
        predict(&log, &read("a"), &chain("a"), &chain("a")),
        Err(Refusal::Replay(7))
    );
    assert_eq!(
        changed(|l| l.push(Edit { pcr: 9, ..appended })),
        Err(Refusal::InitrdEvents)
    );
}

#[test]
fn a_log_that_does_not_replay_is_refused() {
    for n in [4u8, 7, 9] {
        let mut pcrs = read("a");
        let pcr = match n {
            4 => &mut pcrs.pcr4,
            7 => &mut pcrs.pcr7,
            _ => &mut pcrs.pcr9,
        };
        pcr[0] ^= 1;
        assert_eq!(
            predict(A, &pcrs, &chain("a"), &chain("b")),
            Err(Refusal::Replay(n))
        );
    }
    // A changed digest in the log is refused the same way.
    let mut list = events(A);
    let at = position(&list, 7, EV_SEPARATOR);
    list[at].sha256[0] ^= 1;
    assert_eq!(
        predict(&encode(A, &list), &read("a"), &chain("a"), &chain("b")),
        Err(Refusal::Replay(7))
    );
}

#[test]
fn events_that_may_not_recur_are_refused() {
    let application = |l: &Vec<Edit>| position(l, 4, EV_EFI_BOOT_SERVICES_APPLICATION);
    // A boot attempt that returned, logged where OVMF logs it.
    assert_eq!(
        changed(|l| {
            let at = application(l);
            let returned = action(4, RETURNING);
            let calling = action(4, CALLING);
            l.insert(at + 1, returned);
            l.insert(at + 2, calling);
        }),
        Err(Refusal::Returned)
    );
    // A second application: a menu, shell or one-time entry.
    assert_eq!(
        changed(|l| {
            let at = application(l);
            let mut other = l[at].clone();
            other.sha256 = [7; 32];
            l.insert(at, other);
        }),
        Err(Refusal::SecondApplication)
    );
    assert_eq!(
        changed(|l| {
            let at = application(l);
            let again = l[at].clone();
            l.insert(at, again);
        }),
        Err(Refusal::SecondApplication)
    );
    assert_eq!(
        changed(|l| {
            let at = application(l);
            l.remove(at);
        }),
        Err(Refusal::NoApplication)
    );
    // Any other PCR 4 event.
    for other in [
        action(4, b"Some other action"),
        Edit {
            pcr: 4,
            kind: 0xd,
            sha256: [1; 32],
            data: b"IPL".to_vec(),
        },
        Edit {
            pcr: 4,
            kind: 0x8000_0004,
            sha256: [2; 32],
            data: Vec::new(),
        },
    ] {
        assert_eq!(changed(|l| l.push(other)), Err(Refusal::Pcr4Event));
    }
    let calling = |l: &Vec<Edit>| {
        l.iter()
            .position(|e| e.pcr == 4 && e.kind == EV_EFI_ACTION && e.data == CALLING)
            .unwrap()
    };
    assert_eq!(
        changed(|l| {
            let at = calling(l);
            l.remove(at);
        }),
        Err(Refusal::CallingAction)
    );
    assert_eq!(
        changed(|l| {
            let at = calling(l);
            let again = l[at].clone();
            l.insert(at, again);
        }),
        Err(Refusal::CallingAction)
    );
    let separator = |l: &Vec<Edit>| position(l, 4, EV_SEPARATOR);
    assert_eq!(
        changed(|l| {
            let at = separator(l);
            l.remove(at);
        }),
        Err(Refusal::Separator)
    );
    assert_eq!(
        changed(|l| {
            let at = separator(l);
            let again = l[at].clone();
            l.insert(at, again);
        }),
        Err(Refusal::Separator)
    );
}

#[test]
fn pcr_9_must_hold_exactly_the_running_initramfs() {
    let initrd = |l: &Vec<Edit>| position(l, 9, EV_EVENT_TAG);
    // Nonempty load options add the stub's second PCR 9 event.
    assert_eq!(
        changed(|l| {
            let at = initrd(l);
            let options = b"\xed\x22\x3b\x8f\x1a\x00\x00\x00LOADED_IMAGE::LoadOptions\0".to_vec();
            l.insert(
                at,
                Edit {
                    pcr: 9,
                    kind: EV_EVENT_TAG,
                    sha256: [3; 32],
                    data: options,
                },
            );
        }),
        Err(Refusal::InitrdEvents)
    );
    assert_eq!(
        changed(|l| {
            let at = initrd(l);
            let again = l[at].clone();
            l.insert(at, again);
        }),
        Err(Refusal::InitrdEvents)
    );
    assert_eq!(
        changed(|l| {
            let at = initrd(l);
            l.remove(at);
        }),
        Err(Refusal::InitrdEvents)
    );
    assert_eq!(
        changed(|l| {
            let at = initrd(l);
            l[at].data[0] ^= 1;
        }),
        Err(Refusal::InitrdEvent)
    );
    assert_eq!(
        changed(|l| {
            let at = initrd(l);
            l[at].kind = EV_EFI_ACTION;
        }),
        Err(Refusal::InitrdEvent)
    );
}

#[test]
fn the_running_chain_must_be_the_one_measured() {
    let mut other = chain("a");
    other.application = chain("b").application;
    assert_eq!(
        predict(A, &read("a"), &other, &chain("b")),
        Err(Refusal::RunningApplication)
    );
    let mut other = chain("a");
    other.initrd = chain("b").initrd;
    assert_eq!(
        predict(A, &read("a"), &other, &chain("b")),
        Err(Refusal::RunningInitrd)
    );
}

#[test]
fn a_log_without_sha_256_is_refused() {
    // A Spec ID event listing SHA-1 alone.
    let mut spec = SPEC_ID_SIGNATURE.to_vec();
    spec.extend_from_slice(&[0, 0, 0, 0, 0, 2, 0, 2]);
    spec.extend_from_slice(&1u32.to_le_bytes());
    spec.extend_from_slice(&[0x04, 0x00, 20, 0]);
    spec.push(0);
    let mut log = vec![0u8; 4];
    log.extend_from_slice(&EV_NO_ACTION.to_le_bytes());
    log.extend_from_slice(&[0; 20]);
    log.extend_from_slice(&(spec.len() as u32).to_le_bytes());
    log.extend_from_slice(&spec);
    assert_eq!(parse(&log).unwrap_err(), Refusal::NoSha256);
    // An event with no SHA-256 digest in a log that lists SHA-256.
    let algorithms = parse(A).unwrap().algorithms;
    if let Some((id, size)) = algorithms
        .iter()
        .find(|(id, _)| *id != TPM_ALG_SHA256)
        .copied()
    {
        let mut log = header(A).to_vec();
        log.extend_from_slice(&4u32.to_le_bytes());
        log.extend_from_slice(&EV_SEPARATOR.to_le_bytes());
        log.extend_from_slice(&1u32.to_le_bytes());
        log.extend_from_slice(&id.to_le_bytes());
        log.extend_from_slice(&vec![0; size]);
        log.extend_from_slice(&4u32.to_le_bytes());
        log.extend_from_slice(&[0; 4]);
        assert_eq!(parse(&log).unwrap_err(), Refusal::NoSha256);
    }
}

#[test]
fn a_truncated_log_is_refused_at_every_cut() {
    let boundaries = parse(A).unwrap().events.len();
    let mut whole = 0;
    for cut in 1..A.len() {
        match parse(&A[..cut]) {
            Ok(_) => whole += 1,
            Err(Refusal::Truncated) => {}
            Err(other) => panic!("cut {cut}: {other:?}"),
        }
    }
    // A cut at an event boundary leaves a whole, shorter log, which the
    // rules or the replay then refuse.
    assert_eq!(whole, boundaries);
    let last = A.len() - 1;
    assert_eq!(
        predict(&A[..last], &read("a"), &chain("a"), &chain("b")),
        Err(Refusal::Truncated)
    );
}

#[test]
fn an_empty_or_oversized_log_is_refused() {
    assert_eq!(parse(&[]).unwrap_err(), Refusal::Empty);
    let mut big = A.to_vec();
    big.resize(MAX_LOG_BYTES + 1, 0);
    assert_eq!(parse(&big).unwrap_err(), Refusal::Oversized);
    let dir = std::env::temp_dir().join(format!("td-boot-eventlog-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("log");
    std::fs::write(&path, &big).unwrap();
    assert_eq!(read_log(&path), Err(Refusal::Oversized));
    std::fs::write(&path, A).unwrap();
    assert_eq!(read_log(&path).as_deref(), Ok(A));
    assert_eq!(read_log(&dir.join("absent")), Err(Refusal::Unreadable));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn malformed_logs_are_refused() {
    let list = events(A);
    let mut bad = encode(A, &list);
    let first = header(A).len();
    // An event's PCR index past the last PCR.
    bad[first..first + 4].copy_from_slice(&24u32.to_le_bytes());
    assert!(matches!(parse(&bad), Err(Refusal::Malformed(_))));
    // A digest count of zero, and one above the algorithms listed.
    for count in [0u32, 9] {
        let mut bad = encode(A, &list);
        bad[first + 8..first + 12].copy_from_slice(&count.to_le_bytes());
        assert!(matches!(parse(&bad), Err(Refusal::Malformed(_))), "{count}");
    }
    // A digest of an algorithm the Spec ID event does not list.
    let mut bad = encode(A, &list);
    bad[first + 12..first + 14].copy_from_slice(&0x0012u16.to_le_bytes());
    assert!(matches!(parse(&bad), Err(Refusal::Malformed(_))));
    // The Spec ID event: not first, another signature, another version.
    let mut bad = A.to_vec();
    bad[4..8].copy_from_slice(&EV_SEPARATOR.to_le_bytes());
    assert!(matches!(parse(&bad), Err(Refusal::SpecId(_))));
    let mut bad = A.to_vec();
    bad[32 + 14] = b'2';
    assert!(matches!(parse(&bad), Err(Refusal::SpecId(_))));
    let mut bad = A.to_vec();
    bad[32 + 21] = 1;
    assert!(matches!(parse(&bad), Err(Refusal::SpecId(_))));
}
