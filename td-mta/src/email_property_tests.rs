#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::{admission::work::Stop, ports::Deadline};
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
fn complete<'a>(cursor: &mut Cursor<'a>, meter: &mut Meter) -> Result<Option<Property<'a>>, Error> {
    for _ in 0..10000 {
        let before = meter.remaining();
        let result = cursor.poll(Tick(1), meter);
        let after = meter.remaining();
        assert!(before.io_bytes - after.io_bytes <= 256);
        assert!(before.records - after.records <= 32);
        assert_eq!(before.output_bytes, after.output_bytes);
        assert_eq!(before.unlinks, after.unlinks);
        match result? {
            Status::Yield => (),
            Status::Complete(value) => return Ok(value),
        }
    }
    panic!("Email property recognition did not finish")
}
fn parse(name: &str) -> Result<Option<Property<'_>>, Error> {
    complete(&mut Cursor::new(name), &mut work())
}
#[test]
fn stored_and_derived_fields_are_recognized_without_header_alias_collisions() {
    let expected = [
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
    for (name, field) in expected {
        assert_eq!(parse(name), Ok(Some(Property::Field(field))));
    }
    for name in [
        "",
        "Id",
        "blobid",
        "receivedat",
        "preview ",
        "bodyProperties",
        "fetchTextBodyValues",
        "maxBodyValueBytes",
        "partId",
        "subParts",
        "inThread",
        "é",
        "From",
        "Subject",
        "messageID",
        "SentAt",
        "ReplyTo",
        "Header:From",
        "HEADER:Subject:asText",
        "header",
    ] {
        assert_eq!(parse(name), Ok(None), "{name}");
    }
}
#[test]
fn all_aliases_and_parameterized_forms_preserve_requested_spelling() {
    use header_property::{Form, Occurrence};
    let aliases = [
        ("messageId", "Message-ID", Form::MessageIds),
        ("inReplyTo", "In-Reply-To", Form::MessageIds),
        ("references", "References", Form::MessageIds),
        ("sender", "Sender", Form::Addresses),
        ("from", "From", Form::Addresses),
        ("to", "To", Form::Addresses),
        ("cc", "Cc", Form::Addresses),
        ("bcc", "Bcc", Form::Addresses),
        ("replyTo", "Reply-To", Form::Addresses),
        ("subject", "Subject", Form::Text),
        ("sentAt", "Date", Form::Date),
    ];
    for (requested, name, form) in aliases {
        let Some(Property::Header(property)) = parse(requested).unwrap() else {
            panic!("missing alias")
        };
        assert_eq!(
            (
                property.requested(),
                property.name(),
                property.form(),
                property.occurrence()
            ),
            (requested, name, form, Occurrence::Last)
        );
    }
    for (requested, name, form, occurrence) in [
        (
            "header:sUbJeCt:asText:all",
            "sUbJeCt",
            Form::Text,
            Occurrence::All,
        ),
        (
            "header:From:asGroupedAddresses",
            "From",
            Form::GroupedAddresses,
            Occurrence::Last,
        ),
        (
            "header:X-Custom:asURLs",
            "X-Custom",
            Form::URLs,
            Occurrence::Last,
        ),
        (
            "header:Content-Type",
            "Content-Type",
            Form::Raw,
            Occurrence::Last,
        ),
    ] {
        let Some(Property::Header(property)) = parse(requested).unwrap() else {
            panic!("missing header")
        };
        assert_eq!(
            (
                property.requested(),
                property.name(),
                property.form(),
                property.occurrence()
            ),
            (requested, name, form, occurrence)
        );
        assert_eq!(property.requested().as_ptr(), requested.as_ptr());
    }
    assert_eq!(parse("header:From:asDate"), Err(Error::ForbiddenForm));
    for name in [
        "header:",
        "header:X:asBroken",
        "header:X:all:asText",
        "header:é",
    ] {
        assert_eq!(parse(name), Err(Error::InvalidProperty));
    }
}
#[test]
fn complete_results_are_passive_and_every_interrupted_prefix_is_sticky() {
    let long = format!("header:{}:asText", "X".repeat(256));
    for name in [
        "id",
        "preview",
        "subject",
        "header:From:asAddresses",
        "unknown",
        "header:From:asDate",
        &long,
    ] {
        let mut cursor = Cursor::new(name);
        let mut meter = work();
        let expected = complete(&mut cursor, &mut meter);
        let remaining = meter.remaining();
        assert_eq!(
            cursor.poll(Tick(100), &mut meter),
            expected.map(Status::Complete)
        );
        assert_eq!(meter.remaining(), remaining);
        let mut reached_end = false;
        for cutoff in 0..100 {
            let mut cursor = Cursor::new(name);
            let mut meter = work();
            for _ in 0..cutoff {
                if !matches!(cursor.poll(Tick(1), &mut meter), Ok(Status::Yield)) {
                    reached_end = true;
                    break;
                }
            }
            if reached_end {
                break;
            }
            let before = meter.remaining();
            assert_eq!(
                cursor.poll(Tick(100), &mut meter),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(meter.remaining(), before);
            let mut fresh = work();
            let before = fresh.remaining();
            assert_eq!(
                cursor.poll(Tick(1), &mut fresh),
                Err(Error::Work(Stop::Deadline))
            );
            assert_eq!(fresh.remaining(), before);
        }
        assert!(reached_end, "prefix cap missed completion: {name}");
    }
}
fn advance_to_fields(cursor: &mut Cursor<'_>, meter: &mut Meter) {
    for _ in 0..100 {
        if matches!(cursor.phase, Phase::Fields) {
            return;
        }
        assert_eq!(cursor.poll(Tick(1), meter), Ok(Status::Yield));
    }
    panic!("header-to-field transition stalled");
}
#[test]
fn fixed_field_admission_is_atomic_and_long_headers_yield() {
    let mut cursor = Cursor::new("preview");
    let mut meter = work();
    let initial = meter.remaining();
    advance_to_fields(&mut cursor, &mut meter);
    let used_bytes = initial.io_bytes - meter.remaining().io_bytes;
    let used_records = initial.records - meter.remaining().records;
    assert_eq!(
        cursor.poll(Tick(1), &mut meter),
        Ok(Status::Complete(Some(Property::Field(Field::Preview))))
    );
    for (bytes, records, expected) in [
        (FIELD_BYTES, FIELDS.len() as u64, None),
        (FIELD_BYTES - 1, FIELDS.len() as u64, Some(Stop::IoBytes)),
        (FIELD_BYTES, FIELDS.len() as u64 - 1, Some(Stop::Records)),
    ] {
        let mut cursor = Cursor::new("preview");
        let mut meter = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: used_bytes + bytes,
                records: used_records + records,
                ..Charge::default()
            },
        );
        advance_to_fields(&mut cursor, &mut meter);
        let before = meter.remaining();
        let result = cursor.poll(Tick(1), &mut meter);
        if let Some(stop) = expected {
            assert_eq!(result, Err(Error::Work(stop)));
            assert_eq!(meter.remaining(), before);
            let mut fresh = work();
            let before = fresh.remaining();
            assert_eq!(cursor.poll(Tick(1), &mut fresh), result);
            assert_eq!(fresh.remaining(), before);
        } else {
            assert_eq!(
                result,
                Ok(Status::Complete(Some(Property::Field(Field::Preview))))
            );
        }
    }
    let source = format!("header:{}:asText:all", "X".repeat(65536));
    let mut cursor = Cursor::new(&source);
    let mut meter = work();
    let Some(Property::Header(property)) = complete(&mut cursor, &mut meter).unwrap() else {
        panic!("missing long header")
    };
    assert_eq!(property.name().len(), 65536);
    assert_eq!(property.requested().as_ptr(), source.as_ptr());
}
