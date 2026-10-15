//! Borrowed td-mail submission creation and success-filing input. No authority.
use crate::{
    admission::work::{Charge, Meter, Stop},
    ports::{Clock, Deadline, Tick},
    wire,
};
use td_json::arena::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Property(&'static str),
    Capacity,
    Deadline,
    Clock,
}
#[derive(Clone, Copy, Debug)]
pub enum Email<'a> {
    Id(wire::Id<'a>),
    Created(wire::Id<'a>),
}
#[derive(Clone, Copy, Debug)]
pub struct Creation<'a> {
    pub email: Email<'a>,
    pub identity: wire::Id<'a>,
}
/// Always removes $draft. A move must preserve both patch operands: the
/// eventual publisher must not silently replace a different current mailbox.
#[derive(Clone, Copy, Debug)]
pub struct Filing<'a> {
    pub source: Option<wire::Id<'a>>,
    pub destination: Option<wire::Id<'a>>,
}
struct Work<'a> {
    clock: &'a dyn Clock,
    deadline: Deadline,
    last: Tick,
    meter: &'a mut Meter,
}
impl<'a> Work<'a> {
    fn new(clock: &'a dyn Clock, deadline: Deadline, meter: &'a mut Meter) -> Result<Self, Error> {
        if meter.deadline() < deadline.into() {
            return Err(Error::Deadline);
        }
        let mut work = Self {
            clock,
            deadline,
            last: clock.sample().map_err(|_| Error::Clock)?.monotonic,
            meter,
        };
        work.charge(0)?;
        Ok(work)
    }
    fn charge(&mut self, bytes: usize) -> Result<(), Error> {
        let now = self.clock.sample().map_err(|_| Error::Clock)?.monotonic;
        if now < self.last {
            return Err(Error::Clock);
        }
        self.last = now;
        if self.deadline.expired(now) {
            return Err(Error::Deadline);
        }
        self.meter
            .charge(
                now,
                Charge {
                    io_bytes: bytes as u64,
                    records: 1,
                    ..Charge::default()
                },
            )
            .map_err(|error| {
                if error == Stop::Deadline {
                    Error::Deadline
                } else {
                    Error::Capacity
                }
            })
    }
    fn text<'v>(&mut self, value: Value<'v>, field: &'static str) -> Result<&'v str, Error> {
        let bytes = value.as_string_bytes().ok_or(Error::Property(field))?;
        if bytes.len() > 256 {
            return Err(Error::Property(field));
        }
        self.charge(bytes.len())?;
        std::str::from_utf8(bytes).map_err(|_| Error::Property(field))
    }
}
/// The call owner separately validates account, state, create cardinality and
/// creation-key binding. Resolving #draft never confers Email access.
pub fn creation<'a>(
    value: Value<'a>,
    clock: &dyn Clock,
    deadline: Deadline,
    meter: &mut Meter,
) -> Result<Creation<'a>, Error> {
    let mut work = Work::new(clock, deadline, meter)?;
    let mut fields = value.as_object().ok_or(Error::Property("create"))?;
    if fields.len() != 2 {
        return Err(Error::Property("create"));
    }
    let (mut email, mut identity) = (None, None);
    while let Some((key, value)) = fields.next_bytes() {
        work.charge(key.len())?;
        match key {
            b"emailId" if email.is_none() => {
                let text = work.text(value, "emailId")?;
                email = Some(if let Some(key) = text.strip_prefix('#') {
                    Email::Created(wire::Id::parse(key).map_err(|_| Error::Property("emailId"))?)
                } else {
                    Email::Id(wire::Id::parse(text).map_err(|_| Error::Property("emailId"))?)
                });
            }
            b"identityId" if identity.is_none() => {
                identity = Some(
                    wire::Id::parse(work.text(value, "identityId")?)
                        .map_err(|_| Error::Property("identityId"))?,
                );
            }
            _ => return Err(Error::Property("create")),
        }
    }
    work.charge(0)?;
    Ok(Creation {
        email: email.ok_or(Error::Property("emailId"))?,
        identity: identity.ok_or(Error::Property("identityId"))?,
    })
}
/// Decode td-mail's one onSuccessUpdateEmail patch. This does not apply it,
/// authorize a mailbox or decide whether a submission succeeded.
pub fn filing<'a>(
    value: Value<'a>,
    clock: &dyn Clock,
    deadline: Deadline,
    meter: &mut Meter,
) -> Result<Filing<'a>, Error> {
    let mut work = Work::new(clock, deadline, meter)?;
    let mut fields = value
        .as_object()
        .ok_or(Error::Property("onSuccessUpdateEmail"))?;
    if !matches!(fields.len(), 1 | 3) {
        return Err(Error::Property("onSuccessUpdateEmail"));
    }
    let (mut cleared, mut source, mut destination) = (false, None, None);
    while let Some((key, value)) = fields.next_bytes() {
        if key.len() > 266 {
            return Err(Error::Property("onSuccessUpdateEmail"));
        }
        work.charge(key.len())?;
        if key == b"keywords/$draft" && value.is_null() && !cleared {
            cleared = true;
        } else if let Some(key) = key.strip_prefix(b"mailboxIds/") {
            let key = std::str::from_utf8(key).map_err(|_| Error::Property("mailboxIds"))?;
            let id = wire::Id::parse(key).map_err(|_| Error::Property("mailboxIds"))?;
            if value.is_null() && source.is_none() {
                source = Some(id);
            } else if value.as_bool() == Some(true) && destination.is_none() {
                destination = Some(id);
            } else {
                return Err(Error::Property("mailboxIds"));
            }
        } else {
            return Err(Error::Property("onSuccessUpdateEmail"));
        }
    }
    if !cleared
        || source.is_some() != destination.is_some()
        || source.zip(destination).is_some_and(|(a, b)| a == b)
    {
        return Err(Error::Property("onSuccessUpdateEmail"));
    }
    work.charge(0)?;
    Ok(Filing {
        source,
        destination,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    struct Now;
    impl Clock for Now {
        fn sample(&self) -> Result<crate::ports::Time, crate::ports::Error> {
            Ok(crate::ports::Time {
                monotonic: Tick(1),
                utc_ms: 0,
            })
        }
    }
    fn parse(text: &str, run: impl FnOnce(Value<'_>, &mut Meter)) {
        let mut bytes = text.as_bytes().to_vec();
        let mut tokens = [td_json::arena::Token::EMPTY; 128];
        let document = td_json::arena::parse(
            &mut bytes,
            &mut tokens,
            td_json::arena::ParseLimits {
                max_bytes: 4096,
                max_depth: 16,
                max_tokens: 128,
            },
        )
        .unwrap();
        let mut meter = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 4096,
                records: 100,
                ..Default::default()
            },
        );
        run(document.root(), &mut meter);
    }
    fn until() -> Deadline {
        Deadline::after(Tick(0), 90).unwrap()
    }
    #[test]
    fn td_mail_creation_and_filing_preserve_references_and_operands() {
        parse(
            r##"{"emailId":"#draft","identityId":"identity"}"##,
            |value, meter| {
                let parsed = creation(value, &Now, until(), meter).unwrap();
                assert!(matches!(parsed.email, Email::Created(id) if id.as_str() == "draft"));
                assert_eq!(parsed.identity.as_str(), "identity");
                assert!(meter.remaining().records < 100);
            },
        );
        parse(
            r#"{"emailId":"email","identityId":"identity"}"#,
            |value, meter| {
                assert!(
                    matches!(creation(value, &Now, until(), meter).unwrap().email, Email::Id(id) if id.as_str() == "email")
                );
            },
        );
        parse(
            r#"{"keywords/$draft":null,"mailboxIds/sent":true,"mailboxIds/drafts":null}"#,
            |value, meter| {
                let parsed = filing(value, &Now, until(), meter).unwrap();
                assert_eq!(parsed.source.unwrap().as_str(), "drafts");
                assert_eq!(parsed.destination.unwrap().as_str(), "sent");
            },
        );
        parse(r#"{"keywords/$draft":null}"#, |value, meter| {
            let parsed = filing(value, &Now, until(), meter).unwrap();
            assert!(parsed.source.is_none() && parsed.destination.is_none());
        });
    }
    #[test]
    fn unsupported_or_incomplete_shapes_refuse() {
        for text in [
            r#"{}"#,
            r##"{"emailId":"#","identityId":"i"}"##,
            r#"{"emailId":"e","identityId":"i","envelope":null}"#,
            r#"{"emailId":null,"identityId":"i"}"#,
        ] {
            parse(text, |value, meter| {
                assert!(creation(value, &Now, until(), meter).is_err())
            });
        }
        for text in [
            r#"{}"#,
            r#"{"keywords/$draft":false}"#,
            r#"{"keywords/$draft":null,"mailboxIds/sent":true}"#,
            r#"{"keywords/$draft":null,"mailboxIds/a":true,"mailboxIds/b":true}"#,
            r#"{"keywords/$seen":null}"#,
        ] {
            parse(text, |value, meter| {
                assert!(filing(value, &Now, until(), meter).is_err())
            });
        }
    }
    #[test]
    fn original_work_and_phase_deadlines_are_enforced() {
        parse(r##"{"emailId":"#draft","identityId":"i"}"##, |value, _| {
            let mut empty = Meter::new(until(), Charge::default());
            assert!(matches!(
                creation(value, &Now, until(), &mut empty),
                Err(Error::Capacity)
            ));
        });
        parse(r#"{"keywords/$draft":null}"#, |value, meter| {
            assert!(matches!(
                filing(value, &Now, Deadline::after(Tick(0), 1).unwrap(), meter),
                Err(Error::Deadline)
            ));
        });
    }
}
