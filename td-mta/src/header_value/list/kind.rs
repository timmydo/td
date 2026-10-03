use super::{Error, Form};
#[cfg(test)]
use crate::admission::work::Charge;
use crate::{
    admission::work::Meter, header_message_ids as ids, header_urls as urls, json_string,
    nfc::HeaderBudget, ports::Tick,
};
#[derive(Clone, Copy)]
pub(in crate::header_value) enum Event {
    Yield,
    Begin,
    Scalar(char),
    End,
    Complete,
}
pub(in crate::header_value) trait Kind {
    type Cursor<'a, 'w>;
    type Mode: Copy;
    type Failure: Copy + PartialEq + Into<Error> + Into<json_string::Error>;
    const FORM: Form;
    const DEFAULT: Self::Mode;
    const SPECIAL: Self::Mode;
    const MALFORMED: Self::Failure;
    fn candidate(length: usize) -> Option<&'static str>;
    fn start<'a, 'w>(
        bytes: &'a [u8],
        mode: Self::Mode,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self::Cursor<'a, 'w>;
    fn poll(cursor: &mut Self::Cursor<'_, '_>, now: Tick) -> Result<Event, Self::Failure>;
    fn charge_output(
        cursor: &mut Self::Cursor<'_, '_>,
        now: Tick,
        bytes: u64,
    ) -> Result<(), Self::Failure>;
    fn finish<'a, 'w>(
        cursor: Self::Cursor<'a, 'w>,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Self::Failure>;
    fn finish_malformed<'a, 'w>(
        cursor: Self::Cursor<'a, 'w>,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Self::Failure>;
    fn is_encoding_problem(_: &Self::Cursor<'_, '_>) -> bool {
        false
    }
    #[cfg(test)]
    fn remaining(cursor: &Self::Cursor<'_, '_>) -> (Charge, u64);
}
pub(in crate::header_value) struct Ids;
impl Kind for Ids {
    type Cursor<'a, 'w> = ids::project::Budgeted<'a, 'w>;
    type Mode = ids::Mode;
    type Failure = ids::Error;
    const FORM: Form = Form::MessageIds;
    const DEFAULT: Self::Mode = ids::Mode::Strict;
    const SPECIAL: Self::Mode = ids::Mode::ObsoletePhrases;
    const MALFORMED: Self::Failure = ids::Error::Malformed;
    fn candidate(length: usize) -> Option<&'static str> {
        match length {
            10 => Some("References"),
            11 => Some("In-Reply-To"),
            _ => None,
        }
    }
    fn start<'a, 'w>(
        bytes: &'a [u8],
        mode: Self::Mode,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self::Cursor<'a, 'w> {
        ids::project::Budgeted::new(bytes, mode, work, budget)
    }
    fn poll(cursor: &mut Self::Cursor<'_, '_>, now: Tick) -> Result<Event, Self::Failure> {
        Ok(match cursor.poll(now)? {
            ids::project::Status::Yield => Event::Yield,
            ids::project::Status::Begin => Event::Begin,
            ids::project::Status::Scalar(value) => Event::Scalar(value),
            ids::project::Status::End => Event::End,
            ids::project::Status::Complete => Event::Complete,
        })
    }
    fn charge_output(
        cursor: &mut Self::Cursor<'_, '_>,
        now: Tick,
        bytes: u64,
    ) -> Result<(), Self::Failure> {
        cursor.charge_output(now, bytes)
    }
    fn finish<'a, 'w>(
        cursor: Self::Cursor<'a, 'w>,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Self::Failure> {
        cursor.finish()
    }
    fn finish_malformed<'a, 'w>(
        cursor: Self::Cursor<'a, 'w>,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Self::Failure> {
        cursor.finish_malformed()
    }
    fn is_encoding_problem(cursor: &Self::Cursor<'_, '_>) -> bool {
        cursor.is_encoding_problem()
    }
    #[cfg(test)]
    fn remaining(cursor: &Self::Cursor<'_, '_>) -> (Charge, u64) {
        cursor.remaining()
    }
}
pub(in crate::header_value) struct Urls;
impl Kind for Urls {
    type Cursor<'a, 'w> = urls::Budgeted<'a, 'w>;
    type Mode = urls::Mode;
    type Failure = urls::Error;
    const FORM: Form = Form::URLs;
    const DEFAULT: Self::Mode = urls::Mode::URLs;
    const SPECIAL: Self::Mode = urls::Mode::ListPost;
    const MALFORMED: Self::Failure = urls::Error::Malformed;
    fn candidate(length: usize) -> Option<&'static str> {
        (length == 9).then_some("List-Post")
    }
    fn start<'a, 'w>(
        bytes: &'a [u8],
        mode: Self::Mode,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Self::Cursor<'a, 'w> {
        urls::Budgeted::new(bytes, mode, work, budget)
    }
    fn poll(cursor: &mut Self::Cursor<'_, '_>, now: Tick) -> Result<Event, Self::Failure> {
        Ok(match cursor.poll(now)? {
            urls::Status::Yield => Event::Yield,
            urls::Status::Begin => Event::Begin,
            urls::Status::Byte(value) => Event::Scalar(char::from(value)),
            urls::Status::End => Event::End,
            urls::Status::Complete => Event::Complete,
        })
    }
    fn charge_output(
        cursor: &mut Self::Cursor<'_, '_>,
        now: Tick,
        bytes: u64,
    ) -> Result<(), Self::Failure> {
        cursor.charge_output(now, bytes)
    }
    fn finish<'a, 'w>(
        cursor: Self::Cursor<'a, 'w>,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Self::Failure> {
        cursor.finish()
    }
    fn finish_malformed<'a, 'w>(
        cursor: Self::Cursor<'a, 'w>,
    ) -> Result<(&'w mut Meter, &'w mut HeaderBudget), Self::Failure> {
        cursor.finish_malformed()
    }
    #[cfg(test)]
    fn remaining(cursor: &Self::Cursor<'_, '_>) -> (Charge, u64) {
        cursor.remaining()
    }
}
