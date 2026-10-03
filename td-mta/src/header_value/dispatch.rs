//! One selected header form with the original job/email budgets.
use super::*;

/// Provisional JSON selected by the already-authorized property form.
/// Retain chunks in an unpublished response tail until successful completion
/// and final deadline admission. This owner never changes form or source.
pub struct Cursor<'a, 'w> {
    value: Value<'a, 'w>,
}
// Retain the selected parser inline; a box would allocate on property admission.
#[allow(clippy::large_enum_variant)]
enum Value<'a, 'w> {
    Raw(Raw<'a, 'w>),
    Text(Text<'a, 'w>),
    Addresses(Addresses<'a, 'w>),
    GroupedAddresses(GroupedAddresses<'a, 'w>),
    MessageIds(MessageIds<'a, 'w>),
    Date(Date<'a, 'w>),
    URLs(URLs<'a, 'w>),
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        input: Input<'a>,
        scratch: &'w mut nfc::Scratch,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        let value = match input.property.form() {
            Form::Raw => Value::Raw(Raw::new(
                input.bytes,
                input.base,
                input.header_limit,
                input.property,
                input.source_end,
                work,
                budget,
            )?),
            Form::Text => Value::Text(Text::new(input, scratch, work, budget)?),
            Form::Addresses => Value::Addresses(Addresses::new(input, scratch, work, budget)?),
            Form::GroupedAddresses => {
                Value::GroupedAddresses(GroupedAddresses::new(input, scratch, work, budget)?)
            }
            Form::MessageIds => Value::MessageIds(MessageIds::new(input, work, budget)?),
            Form::Date => Value::Date(Date::new(input, work, budget)?),
            Form::URLs => Value::URLs(URLs::new(input, work, budget)?),
        };
        Ok(Self { value })
    }
    /// Final only after property Complete; no other field contributes.
    pub const fn is_encoding_problem(&self) -> bool {
        match &self.value {
            Value::Raw(value) => value.is_encoding_problem(),
            Value::Text(value) => value.is_encoding_problem(),
            Value::Addresses(value) => value.is_encoding_problem(),
            Value::GroupedAddresses(value) => value.is_encoding_problem(),
            Value::MessageIds(value) => value.is_encoding_problem(),
            Value::Date(_) | Value::URLs(_) => false,
        }
    }
    /// Final only after property Complete; meaningful for Date values.
    pub const fn has_unverified_leap(&self) -> bool {
        match &self.value {
            Value::Date(value) => value.has_unverified_leap(),
            Value::Raw(_)
            | Value::Text(_)
            | Value::Addresses(_)
            | Value::GroupedAddresses(_)
            | Value::MessageIds(_)
            | Value::URLs(_) => false,
        }
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        match &mut self.value {
            Value::Raw(value) => value.check_deadline(now),
            Value::Text(value) => value.check_deadline(now),
            Value::Addresses(value) => value.check_deadline(now),
            Value::GroupedAddresses(value) => value.check_deadline(now),
            Value::MessageIds(value) => value.check_deadline(now),
            Value::Date(value) => value.check_deadline(now),
            Value::URLs(value) => value.check_deadline(now),
        }
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        match &mut self.value {
            Value::Raw(value) => value.poll(now, output),
            Value::Text(value) => value.poll(now, output),
            Value::Addresses(value) => value.poll(now, output),
            Value::GroupedAddresses(value) => value.poll(now, output),
            Value::MessageIds(value) => value.poll(now, output),
            Value::Date(value) => value.poll(now, output),
            Value::URLs(value) => value.poll(now, output),
        }
    }
}
