#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{
    admission::work::{Charge, Stop},
    ports::Deadline,
};
use email_property::Field;
fn work() -> Meter {
    Meter::new(
        Deadline::after(Tick(0), 100).unwrap(),
        Charge {
            io_bytes: 1_000_000,
            records: 1_000_000,
            ..Charge::default()
        },
    )
}
fn complete(cursor: &mut Cursor<'_, '_, '_>, work: &mut Meter) -> Result<(), Error> {
    for _ in 0..100_000 {
        let before = work.remaining();
        let status = cursor.poll(Tick(1), work)?;
        let after = work.remaining();
        assert!(before.io_bytes - after.io_bytes <= 256);
        assert!(before.records - after.records <= 32);
        if status == Status::Complete {
            return Ok(());
        }
    }
    panic!("selection completion");
}
#[test]
fn defaults_and_explicit_empty_match_email_get_not_body_defaults() {
    let mut cells = std::array::from_fn::<_, DEFAULT_HEADER_CELLS, _>(|_| Cell::new());
    let mut cursor = Cursor::new(DEFAULT, &mut cells);
    assert!(cursor.value().is_none());
    complete(&mut cursor, &mut work()).unwrap();
    let view = cursor.finish().unwrap();
    assert_eq!(
        view.headers()
            .iter()
            .map(|c| c.property().unwrap().requested())
            .collect::<Vec<_>>(),
        [
            "messageId",
            "inReplyTo",
            "references",
            "sender",
            "from",
            "to",
            "cc",
            "bcc",
            "replyTo",
            "subject",
            "sentAt"
        ]
    );
    for field in [
        Field::Id,
        Field::BlobId,
        Field::ThreadId,
        Field::MailboxIds,
        Field::Keywords,
        Field::Size,
        Field::ReceivedAt,
        Field::HasAttachment,
        Field::Preview,
        Field::BodyValues,
        Field::TextBody,
        Field::HtmlBody,
        Field::Attachments,
    ] {
        assert!(view.fields().contains(field));
    }
    assert!(!view.fields().contains(Field::Headers));
    assert!(!view.fields().contains(Field::BodyStructure));
    let mut empty = [];
    let view = Cursor::new(&[], &mut empty).finish().unwrap();
    assert_eq!(view.fields(), Fields::id_only());
    assert!(view.headers().is_empty());
}
#[test]
fn fixed_fields_and_requested_header_spellings_deduplicate_exactly() {
    let keys = [
        "headers",
        "bodyStructure",
        "subject",
        "subject",
        "header:Subject:asText",
        "header:subject:asText",
        "header:Subject:asText",
        "size",
        "size",
    ];
    let mut cells = std::array::from_fn::<_, 3, _>(|_| Cell::new());
    let mut cursor = Cursor::new(&keys, &mut cells);
    complete(&mut cursor, &mut work()).unwrap();
    let view = cursor.finish().unwrap();
    assert!(view.fields().contains(Field::Id));
    assert!(view.fields().contains(Field::Headers));
    assert!(view.fields().contains(Field::BodyStructure));
    assert!(view.fields().contains(Field::Size));
    assert_eq!(
        view.headers()
            .iter()
            .map(|c| c.property().unwrap().requested())
            .collect::<Vec<_>>(),
        ["subject", "header:Subject:asText", "header:subject:asText"]
    );
}
#[test]
fn refusal_hides_partial_selection_and_never_renews_failed_work() {
    for (keys, expected) in [
        (&["subject", "unknown"][..], Error::InvalidProperty),
        (
            &["subject", "header:From:asDate"][..],
            Error::Recognition(header_property::Error::ForbiddenForm),
        ),
        (&["subject", "from"][..], Error::Capacity),
    ] {
        let mut cells = [Cell::new()];
        let mut cursor = Cursor::new(keys, &mut cells);
        let error = complete(&mut cursor, &mut work()).unwrap_err();
        assert_eq!(error, expected);
        assert!(cursor.value().is_none());
        let mut replacement = work();
        let before = replacement.remaining();
        assert_eq!(cursor.poll(Tick(1), &mut replacement), Err(error));
        assert_eq!(replacement.remaining(), before);
        assert!(matches!(cursor.finish(), Err(e) if e == error));
    }
    let keys = ["subject"];
    let mut cells = [Cell::new()];
    let mut cursor = Cursor::new(&keys, &mut cells);
    let mut meter = Meter::new(Deadline::after(Tick(0), 100).unwrap(), Charge::default());
    let error = cursor.poll(Tick(1), &mut meter).unwrap_err();
    assert!(matches!(
        error,
        Error::Recognition(header_property::Error::Work(Stop::IoBytes | Stop::Records))
    ));
    assert!(cursor.value().is_none());
    let mut replacement = work();
    let before = replacement.remaining();
    assert_eq!(cursor.poll(Tick(1), &mut replacement), Err(error));
    assert_eq!(replacement.remaining(), before);
    assert!(matches!(cursor.finish(), Err(e) if e == error));
}
#[test]
fn long_duplicate_header_comparisons_yield_and_fund_every_prefix() {
    let name = format!("header:X-{}:asRaw", "a".repeat(8192));
    let keys = [name.as_str(), name.as_str(), "subject"];
    let mut cells = [Cell::new(), Cell::new()];
    let mut cursor = Cursor::new(&keys, &mut cells);
    let mut meter = work();
    complete(&mut cursor, &mut meter).unwrap();
    let view = cursor.finish().unwrap();
    assert_eq!(view.headers().len(), 2);
    assert_eq!(view.headers()[0].property().unwrap().requested(), name);
    let duplicate_cost = 1_000_000 - meter.remaining().io_bytes;
    let baseline_keys = [name.as_str(), "subject"];
    let mut baseline_cells = [Cell::new(), Cell::new()];
    let mut baseline = Cursor::new(&baseline_keys, &mut baseline_cells);
    let mut baseline_work = work();
    complete(&mut baseline, &mut baseline_work).unwrap();
    let baseline_cost = 1_000_000 - baseline_work.remaining().io_bytes;
    let mut parser = email_property::Cursor::new(&name);
    let mut parse_work = work();
    let mut parsed = false;
    for _ in 0..100_000 {
        if matches!(
            parser.poll(Tick(1), &mut parse_work).unwrap(),
            email_property::Status::Complete(Some(_))
        ) {
            parsed = true;
            break;
        }
    }
    assert!(parsed);
    let parse_cost = 1_000_000 - parse_work.remaining().io_bytes;
    assert_eq!(
        duplicate_cost - baseline_cost - parse_cost,
        2 * name.len() as u64
    );
}

#[test]
fn default_capacity_and_singleton_fields_are_exact() {
    let mut cells = std::array::from_fn::<_, { DEFAULT_HEADER_CELLS - 1 }, _>(|_| Cell::new());
    let mut cursor = Cursor::new(DEFAULT, &mut cells);
    assert_eq!(complete(&mut cursor, &mut work()), Err(Error::Capacity));
    let fields = [
        ("id", Field::Id),
        ("blobId", Field::BlobId),
        ("threadId", Field::ThreadId),
        ("mailboxIds", Field::MailboxIds),
        ("keywords", Field::Keywords),
        ("size", Field::Size),
        ("receivedAt", Field::ReceivedAt),
        ("headers", Field::Headers),
        ("bodyStructure", Field::BodyStructure),
        ("bodyValues", Field::BodyValues),
        ("textBody", Field::TextBody),
        ("htmlBody", Field::HtmlBody),
        ("attachments", Field::Attachments),
        ("hasAttachment", Field::HasAttachment),
        ("preview", Field::Preview),
    ];
    for (name, requested) in fields {
        let keys = [name];
        let mut cells = [];
        let mut cursor = Cursor::new(&keys, &mut cells);
        complete(&mut cursor, &mut work()).unwrap();
        let view = cursor.finish().unwrap();
        for (_, field) in fields {
            assert_eq!(
                view.fields().contains(field),
                field == Field::Id || field == requested
            );
        }
    }
}
