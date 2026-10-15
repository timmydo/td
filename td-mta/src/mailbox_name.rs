//! Root-folder name validation shared by storage publication adapters.
use crate::{admission::work::Meter, ports};
pub(crate) fn valid(
    name: &str,
    meter: &mut Meter,
    budget: &mut td_mime::nfc::HeaderBudget,
    scratch: &mut td_mime::nfc::Scratch,
    mut now: impl FnMut() -> Result<ports::Tick, ports::Error>,
) -> Result<bool, ports::Error> {
    if name.is_empty()
        || name.len() > crate::format::row::MAX_MAILBOX_NAME
        || name
            .chars()
            .any(|value| value.is_control() || td_mime::unicode::is_noncharacter(value))
    {
        return Ok(false);
    }
    let mut expected = name.chars();
    let mut cursor = td_mime::nfc::Cursor::new(name, scratch, meter, budget);
    loop {
        let time = now()?;
        match cursor.poll(time).map_err(|error| match error {
            td_mime::nfc::Error::Work(crate::admission::work::Stop::Deadline) => {
                ports::Error::Deadline
            }
            td_mime::nfc::Error::Work(_) | td_mime::nfc::Error::InterpretationLimit => {
                ports::Error::Capacity
            }
            _ => ports::Error::Invalid,
        })? {
            td_mime::nfc::Status::Scalar(value) if expected.next() != Some(value) => {
                return Ok(false)
            }
            td_mime::nfc::Status::Scalar(_) | td_mime::nfc::Status::Yield => (),
            td_mime::nfc::Status::Complete => {
                now()?;
                return Ok(expected.next().is_none());
            }
        }
    }
}
