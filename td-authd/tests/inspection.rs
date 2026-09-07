#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;
use std::io::Write;
use std::os::fd::AsFd;

#[test]
#[ignore = "exec-only private inspection reply fixture"]
fn child() {
    let mut wire = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned().unwrap());
    let mut selector = [0];
    wire.read_exact(&mut selector).unwrap();
    match selector[0] {
        0..=3 => wire.write_all(&[0x17, selector[0]]).unwrap(),
        4 => wire.write_all(&[0x17, 4]).unwrap(),
        5 => wire.write_all(&[0x17]).unwrap(),
        6 => wire.write_all(&[0x17, 2, 0]).unwrap(),
        7 => {
            wire.write_all(&[0x17, 2]).unwrap();
            std::process::exit(7);
        }
        8 => std::thread::sleep(Duration::from_secs(30)),
        9 => {
            wire.write_all(&[0x17, 2]).unwrap();
            std::thread::sleep(Duration::from_secs(30));
        }
        _ => panic!("unknown fixture"),
    }
}

pub(crate) fn fixture(selector: u8) -> Inspection {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "inspection::tests::child", "--ignored"]);
    let mut inspection = Inspection::spawn(command).unwrap();
    inspection
        .wire
        .as_mut()
        .unwrap()
        .write_all(&[selector])
        .unwrap();
    inspection
}

fn finish(inspection: &mut Inspection) -> Event {
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        let result = inspection.poll().unwrap();
        if result != Event::Waiting {
            return result;
        }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn bounded_result_needs_eof_and_successful_observed_exit() {
    for selector in 0..=7 {
        let mut operation = fixture(selector);
        let result = finish(&mut operation);
        if selector <= 3 {
            assert_eq!(
                result,
                Event::State(State::decode(&[0x17, selector]).unwrap())
            );
        } else {
            assert_eq!(result, Event::Unavailable);
        }
        assert!(operation.child.is_none());
        assert_eq!(operation.poll().unwrap(), result);
    }
}

#[test]
fn stalled_or_late_results_are_killed_and_reaped_before_unavailable() {
    for selector in [8, 9] {
        let mut operation = fixture(selector);
        operation.deadline = Instant::now();
        assert_eq!(finish(&mut operation), Event::Unavailable);
        assert!(operation.child.is_none());
    }
    let mut late = fixture(2);
    std::thread::sleep(Duration::from_millis(100));
    late.deadline = Instant::now();
    assert_eq!(finish(&mut late), Event::Unavailable);
    assert!(late.child.is_none());
}

#[test]
fn complete_bytes_alone_and_an_open_endpoint_never_report_state() {
    let (parent, _held) = UnixStream::pair().unwrap();
    parent.set_nonblocking(true).unwrap();
    let mut operation = Inspection {
        child: None,
        wire: Some(parent),
        bytes: vec![0x17, 3],
        eof: false,
        deadline: Instant::now() + LIFETIME,
        failed: false,
        terminal: None,
    };
    assert_eq!(operation.poll().unwrap(), Event::Waiting);
    operation.deadline = Instant::now();
    assert_eq!(operation.poll().unwrap(), Event::Unavailable);
}

#[test]
fn production_factory_refuses_wrong_owner_missing_binary_and_missing_reply() {
    assert!(Inspection::start(1001).is_err());
    assert!(Inspection::spawn(Command::new("/missing-inspection-fixture")).is_err());
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--list"]);
    let mut operation = Inspection::spawn(command).unwrap();
    assert_eq!(finish(&mut operation), Event::Unavailable);
    assert!(operation.child.is_none());
}
