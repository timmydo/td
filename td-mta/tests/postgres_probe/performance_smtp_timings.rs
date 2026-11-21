//! Pair a client's dot-to-250 sample with the work on that one transaction.
use super::{message, report, sql, Attempt, Fixture, Knobs, Service, SocketAddr, Stream};
use td_json::Json;

const PHASES: &[&str] = &[
    "smtp_authentication_wait",
    "smtp_commit_wait",
    "smtp_prepare",
    "smtp_publish",
    "smtp_database_commit",
    "smtp_commit",
    "smtp_final_reply",
];

pub(super) fn measure(fixture: &Fixture, cloud: &Service, smtp: SocketAddr, knobs: &Knobs) {
    assert_eq!(knobs.streams, 1, "per-message timing needs one SMTP stream");
    assert!(knobs.messages <= td_mta::limits::SMTP_RATE_MESSAGE_BURST);
    let count = || {
        sql(fixture, "SELECT count(*) FROM td_mta.emails")
            .parse::<u64>()
            .unwrap()
    };
    let initial = count();
    let (mut stream, _) = Stream::connect(smtp);
    let mut before = cloud.phases();
    for index in 0..knobs.messages {
        let body = message(0, index, knobs.large_bytes);
        let Attempt::Accepted(dot_ms) = stream.attempt(&body) else {
            panic!("message {index} did not receive 250 on its first attempt");
        };
        // The peer can read 250 before the server acknowledges its transport flush.
        assert_eq!(stream.command(b"NOOP\r\n"), Some(250));
        let after = cloud.phases();
        assert!(before["process_id"].as_str().is_some());
        assert_eq!(before["process_id"], after["process_id"]);
        let mut fields = format!(
            "\"index\":{index},\"bytes\":{},\"dot_to_250_us\":{:.0}",
            body.len(),
            dot_ms * 1000.0
        );
        for &phase in PHASES {
            use std::fmt::Write;
            let delta = |field: &str| {
                let value = |sample: &Json| sample["phases"][phase][field].as_u64().unwrap();
                value(&after).checked_sub(value(&before)).unwrap()
            };
            assert_eq!(delta("calls"), 1, "message {index} {phase}");
            for field in ["failures", "busy", "unavailable", "lost"] {
                assert_eq!(delta(field), 0, "message {index} {phase} {field}");
            }
            write!(fields, ",\"{phase}_us\":{}", delta("total_us")).unwrap();
        }
        report("smtp_transaction", &fields);
        before = after;
    }
    assert_eq!(stream.command(b"QUIT\r\n"), Some(221));
    assert_eq!(count(), initial + knobs.messages);
}
