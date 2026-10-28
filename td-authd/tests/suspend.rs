#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;

#[derive(Default)]
struct Fake {
    writes: Vec<(String, Vec<u8>)>,
    fail: Option<&'static str>,
    state: Option<&'static str>,
    count: Option<&'static str>,
}
impl Power for Fake {
    fn read(&mut self, name: &str) -> Result<String, String> {
        Ok(match name {
            "state" => self.state.unwrap_or("freeze mem\n"),
            "mem_sleep" => "[s2idle] deep\n",
            "wakeup_count" => self.count.unwrap_or("42\n"),
            _ => panic!("unexpected read {name}"),
        }
        .into())
    }
    fn write(&mut self, name: &str, bytes: &[u8]) -> Result<(), String> {
        self.writes.push((name.into(), bytes.to_vec()));
        if self.fail == Some(name) {
            Err("injected failure".into())
        } else {
            Ok(())
        }
    }
}

#[test]
fn entry_selects_s2idle_and_arms_exact_wakeup_count_before_mem() {
    let mut fake = Fake::default();
    cycle(&mut fake).unwrap();
    assert_eq!(
        fake.writes,
        vec![
            ("mem_sleep".into(), b"s2idle".to_vec()),
            ("wakeup_count".into(), b"42".to_vec()),
            ("state".into(), b"mem".to_vec()),
        ]
    );
}

#[test]
fn every_failed_step_refuses_entry_without_retrying() {
    for (index, step) in ["mem_sleep", "wakeup_count", "state"].iter().enumerate() {
        let mut fake = Fake {
            fail: Some(step),
            ..Fake::default()
        };
        assert!(cycle(&mut fake).is_err());
        assert_eq!(fake.writes.len(), index + 1);
    }
    let mut unsupported = Fake {
        state: Some("freeze\n"),
        ..Fake::default()
    };
    assert!(cycle(&mut unsupported).is_err());
    assert!(unsupported.writes.is_empty());
    for count in ["", "-1", "1 2", "18446744073709551616"] {
        let mut fake = Fake {
            count: Some(count),
            ..Fake::default()
        };
        assert!(cycle(&mut fake).is_err());
        assert_eq!(fake.writes.len(), 1);
    }
}
