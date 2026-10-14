//! Header projection retaining the original collected bytes and native custody.
use crate::{
    admission::work::{Charge, Meter, Stop},
    header_property::Property,
    header_value, mime_headers,
    nfc::{HeaderBudget, Scratch},
    ports::{Error as PolicyError, Tick},
    root_headers::View,
    store_fs::PinnedBlob,
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Read(crate::root_headers::Error),
    Policy(PolicyError),
    Work(Stop),
    Value(header_value::Error),
    Headers(mime_headers::json::Error),
    State,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(e) => write!(f, "header input: {e}"),
            Self::Policy(e) => write!(f, "header source: {e}"),
            Self::Work(e) => write!(f, "header projection work: {e}"),
            Self::Value(e) => write!(f, "header value: {e}"),
            Self::Headers(e) => write!(f, "header array: {e}"),
            Self::State => f.write_str("invalid header projection state"),
        }
    }
}
impl std::error::Error for Error {}
/// Original source and budgets held across successive header properties.
/// Account authorization and response retention remain the caller's obligations.
/// ```compile_fail,E0277
/// fn bound<T: Clone>() {} bound::<td_mta::mime_response::root_header_json::Input<'_, '_, '_, '_, '_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn bound<T: Copy>() {} bound::<td_mta::mime_response::root_header_json::Input<'_, '_, '_, '_, '_, '_>>();
/// ```
pub struct Input<'p, 'v, 'c, 'b, 'w, 'h> {
    source: &'p mut PinnedBlob<'v, 'c>,
    work: &'w mut Meter,
    budget: &'h mut HeaderBudget,
    scratch: &'h mut Scratch,
    view: View<'b>,
    header_limit: u64,
    state: State,
}
#[derive(Clone, Copy)]
enum State {
    Idle,
    Open,
    Failed(Error),
}
impl<'p, 'v, 'c, 'b, 'w, 'h> Input<'p, 'v, 'c, 'b, 'w, 'h> {
    pub(crate) fn new(
        reader: crate::root_headers::Cursor<'p, 'v, 'c, 'b, 'w>,
        budget: &'h mut HeaderBudget,
        scratch: &'h mut Scratch,
        now: Tick,
    ) -> Result<Self, Error> {
        let (source, work, view, header_limit) =
            reader.into_json_parts(now).map_err(Error::Read)?;
        Ok(Self {
            source,
            work,
            budget,
            scratch,
            view,
            header_limit,
            state: State::Idle,
        })
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Selection<'a> {
    Headers,
    Value(Property<'a>),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    NeedOutput,
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub written: usize,
    pub status: Status,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Diagnostics {
    pub is_encoding_problem: bool,
    pub has_unverified_leap: bool,
}
// Keep the selected parser inline; property admission does not allocate.
#[allow(clippy::large_enum_variant)]
enum Parser<'a> {
    Value(header_value::Cursor<'a, 'a>),
    Headers(mime_headers::json::Cursor<'a, 'a>),
}
pub struct Cursor<'a, 'v, 'c> {
    source: &'a mut PinnedBlob<'v, 'c>,
    parser: Parser<'a>,
    state: &'a mut State,
    expected: mime_headers::End,
    complete: bool,
}
impl Input<'_, '_, '_, '_, '_, '_> {
    /// Require this final check before publishing any projected properties.
    #[must_use = "publication requires successful final freshness checks"]
    pub fn finish(mut self, now: Tick) -> Result<(), Error> {
        self.check_deadline(now)
    }

    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        match self.state {
            State::Idle => {}
            State::Failed(e) => return Err(e),
            State::Open => {
                self.state = State::Failed(Error::State);
                return Err(Error::State);
            }
        }
        let result = self
            .source
            .check_deadline()
            .map_err(Error::Policy)
            .and_then(|()| {
                self.work
                    .charge(now, Charge::default())
                    .map_err(Error::Work)
            });
        if let Err(e) = result {
            self.state = State::Failed(e);
        }
        result
    }
}
impl<'v, 'c> Input<'_, 'v, 'c, '_, '_, '_> {
    pub fn project<'a>(
        &'a mut self,
        selection: Selection<'a>,
        now: Tick,
    ) -> Result<Cursor<'a, 'v, 'c>, Error> {
        self.check_deadline(now)?;
        let parser = match selection {
            Selection::Headers => mime_headers::json::Cursor::new(
                mime_headers::json::Input {
                    source: self.view.bytes,
                    base: self.view.base,
                    source_end: self.view.source_end,
                    header_limit: self.header_limit,
                },
                self.work,
                self.budget,
                now,
            )
            .map(Parser::Headers)
            .map_err(Error::Headers),
            Selection::Value(property) => header_value::Cursor::new(
                header_value::Input {
                    bytes: self.view.bytes,
                    base: self.view.base,
                    source_end: self.view.source_end,
                    header_limit: self.header_limit,
                    property,
                },
                self.scratch,
                self.work,
                self.budget,
            )
            .map(Parser::Value)
            .map_err(Error::Value),
        };
        let parser = match parser {
            Ok(p) => p,
            Err(e) => {
                self.state = State::Failed(e);
                return Err(e);
            }
        };
        self.state = State::Open;
        Ok(Cursor {
            source: self.source,
            parser,
            state: &mut self.state,
            expected: self.view.end,
            complete: false,
        })
    }
}
impl Cursor<'_, '_, '_> {
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        match *self.state {
            State::Open => {}
            State::Failed(e) => return Err(e),
            State::Idle => {
                *self.state = State::Failed(Error::State);
                return Err(Error::State);
            }
        }
        let result = self
            .source
            .check_deadline()
            .map_err(Error::Policy)
            .and_then(|()| match &mut self.parser {
                Parser::Value(p) => p.check_deadline(now).map_err(Error::Value),
                Parser::Headers(p) => p.check_deadline(now).map_err(Error::Headers),
            });
        if let Err(e) = result {
            *self.state = State::Failed(e);
        }
        result
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.check_deadline(now)?;
        if self.complete {
            return Ok(Progress {
                written: 0,
                status: Status::Complete,
            });
        }
        let result = match &mut self.parser {
            Parser::Value(p) => p
                .poll(now, output)
                .map_err(Error::Value)
                .and_then(|progress| {
                    let status = match progress.status {
                        header_value::Status::Yield => Status::Yield,
                        header_value::Status::NeedOutput => Status::NeedOutput,
                        header_value::Status::Complete(end) if end == self.expected => {
                            Status::Complete
                        }
                        header_value::Status::Complete(_) => return Err(Error::State),
                    };
                    Ok(Progress {
                        written: progress.written,
                        status,
                    })
                }),
            Parser::Headers(p) => {
                p.poll(now, output)
                    .map_err(Error::Headers)
                    .and_then(|progress| {
                        let status = match progress.status {
                            crate::json_string::Status::Yield => Status::Yield,
                            crate::json_string::Status::NeedOutput => Status::NeedOutput,
                            crate::json_string::Status::Complete => {
                                if p.value().map(|v| v.0) != Some(self.expected) {
                                    return Err(Error::State);
                                }
                                Status::Complete
                            }
                        };
                        Ok(Progress {
                            written: progress.written,
                            status,
                        })
                    })
            }
        };
        let progress = match result {
            Ok(p) => p,
            Err(e) => {
                *self.state = State::Failed(e);
                return Err(e);
            }
        };
        self.check_deadline(now)?;
        self.complete = progress.status == Status::Complete;
        Ok(progress)
    }
    /// All prior bytes stay provisional until this fresh completion succeeds.
    pub fn finish(mut self, now: Tick) -> Result<Diagnostics, Error> {
        self.check_deadline(now)?;
        if !self.complete {
            *self.state = State::Failed(Error::State);
            return Err(Error::State);
        }
        let diagnostics = match &self.parser {
            Parser::Value(p) => Diagnostics {
                is_encoding_problem: p.is_encoding_problem(),
                has_unverified_leap: p.has_unverified_leap(),
            },
            Parser::Headers(p) => {
                let Some((_, is_encoding_problem)) = p.value() else {
                    *self.state = State::Failed(Error::State);
                    return Err(Error::State);
                };
                Diagnostics {
                    is_encoding_problem,
                    has_unverified_leap: false,
                }
            }
        };
        *self.state = State::Idle;
        Ok(diagnostics)
    }
}
impl Drop for Cursor<'_, '_, '_> {
    fn drop(&mut self) {
        if matches!(*self.state, State::Open) {
            *self.state = State::Failed(Error::State);
        }
    }
}
impl<'p, 'v, 'c, 'b, 'w> crate::root_headers::Cursor<'p, 'v, 'c, 'b, 'w> {
    pub fn into_json<'h>(
        self,
        budget: &'h mut crate::nfc::HeaderBudget,
        scratch: &'h mut crate::nfc::Scratch,
        now: Tick,
    ) -> Result<
        crate::mime_response::root_header_json::Input<'p, 'v, 'c, 'b, 'w, 'h>,
        crate::mime_response::root_header_json::Error,
    > {
        crate::mime_response::root_header_json::Input::new(self, budget, scratch, now)
    }
}

#[cfg(test)]
#[path = "root_header_json/tests.rs"]
mod tests;

const _: () = assert!(std::mem::size_of::<Input<'_, '_, '_, '_, '_, '_>>() <= 192);
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_, '_>>() <= 3072);
