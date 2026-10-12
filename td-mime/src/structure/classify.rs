//! Structure classification from a completed selection and retained header bytes.
use super::{Encoding, Error, Media, UNKNOWN_ENCODING};
use crate::{
    decode_work, metadata,
    nfc::{Credit, HeaderBudget},
    time::Tick,
    work::Meter,
};

/// The selection must describe these same immutable header bytes.
/// These passive labels grant no blob or part authority.
pub struct Input<'a> {
    pub source: &'a [u8],
    pub base: u64,
    pub selected: metadata::Selection,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Value {
    pub media: Media,
    pub encoding: Encoding,
    pub type_start: u64,
    pub type_end: u64,
    pub diagnostics: u8,
    pub digest: bool,
}
/// At most 31 byte visits in one turn; source custody and terminal refusal belong
/// to the enclosing owner. Replays spend the same original live budgets.
pub fn classify(
    input: Input<'_>,
    now: Tick,
    work: &mut Meter,
    budget: &mut HeaderBudget,
    credit: &mut Credit,
) -> Result<Value, Error> {
    budget
        .charge(work, now, 0, 0, credit)
        .map_err(decode_work::Error::from)
        .map_err(Error::from)?;
    classify_with(input, |bytes| {
        budget
            .charge(work, now, bytes, bytes, credit)
            .map_err(decode_work::Error::from)
            .map_err(Error::from)
    })
}
pub(super) fn local(
    input: Input<'_>,
    now: Tick,
    work: &mut Meter,
    budget: &mut HeaderBudget,
    credit: &mut u8,
) -> Result<Value, Error> {
    classify_with(input, |bytes| {
        budget
            .charge_local(work, now, bytes, bytes, credit)
            .map_err(decode_work::Error::from)
            .map_err(Error::from)
    })
}
impl<'a> Input<'a> {
    fn head(&self, selected: metadata::Selected, second: bool) -> Result<&'a [u8], Error> {
        let raw = td_header::resident::slice(
            self.source,
            self.base,
            selected.field.value_start..selected.field.value_end,
        )
        .ok_or(Error::InvalidRange)?;
        let extent = if second {
            selected.head.second.ok_or(Error::InvalidState)?
        } else {
            selected.head.first
        };
        raw.get(extent.start..extent.end).ok_or(Error::InvalidState)
    }
}
fn equal(
    source: &[u8],
    wanted: &[u8],
    charge: &mut impl FnMut(u64) -> Result<(), Error>,
) -> Result<bool, Error> {
    if source.len() != wanted.len() {
        return Ok(false);
    }
    charge(u64::try_from(source.len()).map_err(|_| Error::InvalidState)?)?;
    Ok(source.eq_ignore_ascii_case(wanted))
}
fn classify_with(
    input: Input<'_>,
    mut charge: impl FnMut(u64) -> Result<(), Error>,
) -> Result<Value, Error> {
    let mut value = Value {
        media: Media::Other,
        encoding: Encoding::Identity,
        type_start: 0,
        type_end: 0,
        diagnostics: 0,
        digest: false,
    };
    value.media = match input.selected.content_type {
        metadata::ContentType::Default(metadata::DefaultType::TextPlain) => Media::TextPlain,
        metadata::ContentType::Default(metadata::DefaultType::MessageRfc822) => {
            Media::MessageRfc822
        }
        metadata::ContentType::Field(selected) => {
            value.type_start = selected.field.value_start;
            value.type_end = selected.field.value_end;
            let first = input.head(selected, false)?;
            let second = input.head(selected, true)?;
            if equal(first, b"multipart", &mut charge)? {
                value.digest = equal(second, b"digest", &mut charge)?;
                Media::Multipart
            } else if equal(first, b"text", &mut charge)? && equal(second, b"plain", &mut charge)? {
                Media::TextPlain
            } else if equal(first, b"message", &mut charge)?
                && equal(second, b"rfc822", &mut charge)?
            {
                Media::MessageRfc822
            } else {
                Media::Other
            }
        }
    };
    if let Some(selected) = input.selected.transfer_encoding {
        let name = input.head(selected, false)?;
        value.encoding = if equal(name, b"base64", &mut charge)? {
            Encoding::Base64
        } else if equal(name, b"quoted-printable", &mut charge)? {
            Encoding::QuotedPrintable
        } else {
            if !(equal(name, b"7bit", &mut charge)?
                || equal(name, b"8bit", &mut charge)?
                || equal(name, b"binary", &mut charge)?)
            {
                value.diagnostics |= UNKNOWN_ENCODING;
            }
            Encoding::Identity
        };
    }
    Ok(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::{
        header_select::SourceEnd,
        time::Deadline,
        work::{Charge, Stop},
    };
    fn meter(io: u64, records: u64) -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: io,
                records,
                output_bytes: 1_000_000,
                ..Charge::default()
            },
        )
    }
    fn selection(source: &[u8], base: u64, context: metadata::Context) -> metadata::Selection {
        let mut work = meter(1_000_000, 1_000_000);
        let mut cursor =
            metadata::Cursor::new(source, base, 65536, context, SourceEnd::Prefix).unwrap();
        for _ in 0..10000 {
            if cursor.poll(Tick(1), &mut work).unwrap() == metadata::Status::Complete {
                return cursor.selection().unwrap().unwrap();
            }
        }
        panic!("metadata did not finish")
    }
    #[test]
    fn header_only_classification_preserves_defaults_tokens_and_absolute_fields() {
        type Case<'a> = (&'a [u8], metadata::Context, Media, Encoding, bool, u8);
        let cases: &[Case<'_>] = &[
            (b"\n", metadata::Context::Normal, Media::TextPlain, Encoding::Identity, false, 0),
            (b"\n", metadata::Context::DigestChild, Media::MessageRfc822, Encoding::Identity, false, 0),
            (b"Content-Type: bad\n\n", metadata::Context::DigestChild, Media::MessageRfc822, Encoding::Identity, false, 0),
            (b"Content-Type: MULTIPART/DIGEST; boundary=x\nContent-Transfer-Encoding: binary\n\n", metadata::Context::Normal, Media::Multipart, Encoding::Identity, true, 0),
            (b"Content-Type: multipart/mixed; boundary=x\n\n", metadata::Context::Normal, Media::Multipart, Encoding::Identity, false, 0),
            (b"Content-Type: text/PLAIN\nContent-Transfer-Encoding: BASE64\n\n", metadata::Context::Normal, Media::TextPlain, Encoding::Base64, false, 0),
            (b"Content-Type: text/html\nContent-Transfer-Encoding: quoted-printable\n\n", metadata::Context::Normal, Media::Other, Encoding::QuotedPrintable, false, 0),
            (b"Content-Type: message/RFC822\nContent-Transfer-Encoding: 7bit\n\n", metadata::Context::Normal, Media::MessageRfc822, Encoding::Identity, false, 0),
            (b"Content-Type: message/global\nContent-Transfer-Encoding: 8bit\n\n", metadata::Context::Normal, Media::Other, Encoding::Identity, false, 0),
            (b"Content-Type: application/octet-stream\nContent-Transfer-Encoding: unknown\n\n", metadata::Context::Normal, Media::Other, Encoding::Identity, false, UNKNOWN_ENCODING),
        ];
        let costs = [0, 0, 0, 27, 9, 15, 20, 17, 21, 0];
        assert_eq!(cases.len(), costs.len());
        for (&(source, context, media, encoding, digest, diagnostics), expected_cost) in
            cases.iter().zip(costs)
        {
            for base in [0, 17, u64::MAX - source.len() as u64] {
                let selected = selection(source, base, context);
                let mut work = meter(1_000_000, 1_000_000);
                let mut budget = HeaderBudget::new();
                let mut credit = Credit::new();
                let initial = (budget.source_bytes_remaining(), budget.steps_remaining());
                let value = classify(
                    Input {
                        source,
                        base,
                        selected,
                    },
                    Tick(1),
                    &mut work,
                    &mut budget,
                    &mut credit,
                )
                .unwrap();
                assert_eq!(
                    (value.media, value.encoding, value.digest, value.diagnostics),
                    (media, encoding, digest, diagnostics)
                );
                match selected.content_type {
                    metadata::ContentType::Default(_) => {
                        assert_eq!((value.type_start, value.type_end), (0, 0))
                    }
                    metadata::ContentType::Field(field) => assert_eq!(
                        (value.type_start, value.type_end),
                        (field.field.value_start, field.field.value_end)
                    ),
                }
                let used = 1_000_000 - work.remaining().io_bytes;
                assert_eq!(used, expected_cost);
                assert_eq!(initial.0 - budget.source_bytes_remaining(), used);
                assert_eq!(initial.1 - budget.steps_remaining(), used);
                let mut local_work = meter(1_000_000, 1_000_000);
                let mut local_budget = HeaderBudget::new();
                let mut local_credit = 0;
                assert_eq!(
                    local(
                        Input {
                            source,
                            base,
                            selected
                        },
                        Tick(1),
                        &mut local_work,
                        &mut local_budget,
                        &mut local_credit
                    )
                    .unwrap(),
                    value
                );
                assert_eq!(local_work.remaining(), work.remaining());
                assert_eq!(local_credit, credit.remaining());
            }
        }
    }
    #[test]
    fn exact_cost_and_every_funding_cut_are_explicit_refusals() {
        let source =
            b"Content-Type: multipart/digest\nContent-Transfer-Encoding: quoted-printable\n\n";
        let selected = selection(source, 17, metadata::Context::Normal);
        // multipart (9), digest (6), quoted-printable (16): 31 visits, two records.
        for io in 0..=31 {
            for records in 0..=2 {
                let mut work = meter(io, records);
                let mut budget = HeaderBudget::new();
                let result = classify(
                    Input {
                        source,
                        base: 17,
                        selected,
                    },
                    Tick(1),
                    &mut work,
                    &mut budget,
                    &mut Credit::new(),
                );
                if io == 31 && records == 2 {
                    assert_eq!(result.unwrap().media, Media::Multipart);
                    assert_eq!(
                        (work.remaining().io_bytes, work.remaining().records),
                        (0, 0)
                    );
                } else {
                    assert!(matches!(result, Err(Error::Work(_))));
                }
            }
        }
    }
    #[test]
    fn every_aggregate_visit_and_step_cut_refuses_in_both_entries() {
        let source =
            b"Content-Type: multipart/digest\nContent-Transfer-Encoding: quoted-printable\n\n";
        let selected = selection(source, 0, metadata::Context::Normal);
        for public in [false, true] {
            for visits in [false, true] {
                for remaining in 0..=31 {
                    let mut work = meter(100_000_000, 100_000_000);
                    let mut budget = HeaderBudget::new();
                    let mut credit = Credit::new();
                    let mut local_credit = 0;
                    let debit = if visits {
                        (budget.source_bytes_remaining() - remaining, 0)
                    } else {
                        (0, budget.steps_remaining() - remaining)
                    };
                    let input = Input {
                        source,
                        base: 0,
                        selected,
                    };
                    let result = if public {
                        budget
                            .charge(&mut work, Tick(1), debit.0, debit.1, &mut credit)
                            .unwrap();
                        classify(input, Tick(1), &mut work, &mut budget, &mut credit)
                    } else {
                        budget
                            .charge_local(&mut work, Tick(1), debit.0, debit.1, &mut local_credit)
                            .unwrap();
                        local(input, Tick(1), &mut work, &mut budget, &mut local_credit)
                    };
                    if remaining == 31 {
                        assert_eq!(result.unwrap().media, Media::Multipart);
                    } else {
                        assert_eq!(result, Err(Error::InterpretationLimit));
                    }
                }
            }
        }
    }
    #[test]
    fn credit_replacement_and_expired_zero_cost_defaults_refuse() {
        let source = b"Content-Type: text/plain\n\n";
        let selected = selection(source, 0, metadata::Context::Normal);
        let mut work = meter(1000, 1000);
        let mut budget = HeaderBudget::new();
        let mut credit = Credit::new();
        classify(
            Input {
                source,
                base: 0,
                selected,
            },
            Tick(1),
            &mut work,
            &mut budget,
            &mut credit,
        )
        .unwrap();
        let mut replacement = meter(1000, 1000);
        assert_eq!(
            classify(
                Input {
                    source,
                    base: 0,
                    selected
                },
                Tick(1),
                &mut replacement,
                &mut budget,
                &mut credit
            ),
            Err(Error::InvalidState)
        );
        assert_eq!(replacement.remaining().io_bytes, 1000);
        assert_eq!(
            classify(
                Input {
                    source,
                    base: 0,
                    selected
                },
                Tick(1),
                &mut work,
                &mut budget,
                &mut credit
            ),
            Err(Error::InvalidState)
        );
        let selected = selection(b"\n", 0, metadata::Context::Normal);
        assert_eq!(
            classify(
                Input {
                    source: b"\n",
                    base: 0,
                    selected
                },
                Tick(100),
                &mut work,
                &mut budget,
                &mut Credit::new()
            ),
            Err(Error::Work(Stop::Deadline))
        );
    }
    #[test]
    fn missing_selected_bytes_are_not_absence() {
        let source = b"Content-Type: text/plain\n\n";
        let selected = selection(source, 17, metadata::Context::Normal);
        let mut work = meter(1000, 1000);
        let mut budget = HeaderBudget::new();
        assert_eq!(
            classify(
                Input {
                    source: b"",
                    base: 17,
                    selected
                },
                Tick(1),
                &mut work,
                &mut budget,
                &mut Credit::new()
            ),
            Err(Error::InvalidRange)
        );
        let mut invalid = selected;
        if let metadata::ContentType::Field(ref mut field) = invalid.content_type {
            field.head.second = None;
        }
        assert_eq!(
            classify(
                Input {
                    source,
                    base: 17,
                    selected: invalid
                },
                Tick(1),
                &mut work,
                &mut budget,
                &mut Credit::new()
            ),
            Err(Error::InvalidState)
        );
    }
}
