//! Complete part headers and selected label JSON under the original owners.
use crate::{
    admission::work::Meter,
    mime_headers::Field,
    mime_label_fields::json as labels,
    nfc::{self, HeaderBudget, Scratch},
    ports::Tick,
};
/// Independent caller-reserved header and selected-label JSON windows.
pub struct Backing<'w> {
    pub headers: super::Backing<'w>,
    pub labels: labels::Backing<'w>,
}
/// Passive complete metadata; source/blob/response authority remains external.
pub struct View<'w> {
    pub headers: super::View<'w>,
    pub labels: labels::Retained<'w>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Headers(super::Error),
    Labels(labels::Error),
    Admission(nfc::Error),
    InvalidState,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Headers(error) => write!(f, "part label headers: {error}"),
            Self::Labels(error) => write!(f, "part label JSON: {error}"),
            Self::Admission(error) => write!(f, "part label admission: {error}"),
            Self::InvalidState => f.write_str("invalid part label JSON state"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Headers(error) => Some(error),
            Self::Labels(error) => Some(error),
            Self::Admission(error) => Some(error),
            Self::InvalidState => None,
        }
    }
}
pub use super::Status;
// Header and label cursors never coexist as live owners.
#[allow(clippy::large_enum_variant)]
enum Owner<'a, 'w> {
    Headers(super::Cursor<'a, 'w>, labels::Backing<'w>),
    Labels(labels::Cursor<'a, 'w>, &'w mut Scratch),
    Budgets(&'w mut Meter, &'w mut HeaderBudget, &'w mut Scratch),
    Retired,
}
/// Bind one complete authorized entity and separate caller-reserved windows.
/// Selected field extents are mapped only into that same immutable entity.
/// All headers and JSON remain provisional through whole healthy completion.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mta::mime_part_headers::label_json::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mta::mime_part_headers::label_json::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    source: &'a [u8],
    base: u64,
    owner: Owner<'a, 'w>,
    headers: Option<super::View<'w>>,
    labels: Option<labels::Retained<'w>>,
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        entity: super::Entity<'a>,
        backing: Backing<'w>,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
        scratch: &'w mut Scratch,
    ) -> Result<Self, Error> {
        let source = entity.source;
        let base = entity.base;
        let headers = super::Cursor::new(entity, backing.headers, work, budget, scratch)
            .map_err(Error::Headers)?;
        Ok(Self {
            source,
            base,
            owner: Owner::Headers(headers, backing.labels),
            headers: None,
            labels: None,
            failure: None,
        })
    }
    pub fn is_complete(&self) -> bool {
        self.failure.is_none()
            && self.headers.is_some()
            && self.labels.is_some()
            && matches!(self.owner, Owner::Budgets(..))
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
            self.headers = None;
            self.labels = None;
            self.owner = Owner::Retired;
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = match &mut self.owner {
            Owner::Headers(cursor, _) => cursor.check_deadline(now).map_err(Error::Headers),
            Owner::Labels(cursor, _) => cursor.check_deadline(now).map_err(Error::Labels),
            Owner::Budgets(work, budget, _) => budget
                .charge(work, now, 0, 0, &mut 0)
                .map_err(Error::Admission),
            Owner::Retired => Err(Error::InvalidState),
        };
        self.outcome(result)
    }
    fn field(&self, headers: &super::View<'_>, field: Field) -> Result<&'a [u8], Error> {
        let end = self
            .base
            .checked_add(headers.header_bytes)
            .ok_or(Error::InvalidState)?;
        if field.name_start < self.base
            || field.name_start > field.name_end
            || field.name_end > field.value_start
            || field.value_start > field.value_end
            || field.value_end > end
            || end > headers.body_start
        {
            return Err(Error::InvalidState);
        }
        td_header::resident::slice(self.source, self.base, field.value_start..field.value_end)
            .ok_or(Error::InvalidState)
    }
    pub fn value(&self) -> Option<View<'_>> {
        if !self.is_complete() {
            return None;
        }
        let labels = self.labels.as_ref()?;
        Some(View {
            headers: self.headers?,
            labels: labels::Retained {
                content_id: labels.content_id,
                content_id_end: labels.content_id_end,
                content_language: labels.content_language,
            },
        })
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.is_complete() {
            return Ok(Status::Complete);
        }
        self.check_deadline(now)?;
        let result = self.step(now);
        self.outcome(result)
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        match &mut self.owner {
            Owner::Headers(cursor, _) => {
                if cursor.poll(now).map_err(Error::Headers)? != Status::Complete {
                    return Ok(Status::Yield);
                }
                let Owner::Headers(cursor, backing) =
                    std::mem::replace(&mut self.owner, Owner::Retired)
                else {
                    return Err(Error::InvalidState);
                };
                let (headers, work, budget, scratch) =
                    cursor.finish(now).map_err(Error::Headers)?;
                let content_id = headers
                    .content_id_field
                    .map(|field| self.field(&headers, field))
                    .transpose()?;
                let content_language = headers
                    .content_language_field
                    .map(|field| self.field(&headers, field))
                    .transpose()?;
                self.headers = Some(headers);
                self.owner = Owner::Labels(
                    labels::Cursor::new(
                        labels::Values {
                            content_id,
                            content_language,
                        },
                        backing,
                        work,
                        budget,
                    ),
                    scratch,
                );
                Ok(Status::Yield)
            }
            Owner::Labels(cursor, _) => {
                if cursor.poll(now).map_err(Error::Labels)? != labels::Status::Complete {
                    return Ok(Status::Yield);
                }
                let Owner::Labels(cursor, scratch) =
                    std::mem::replace(&mut self.owner, Owner::Retired)
                else {
                    return Err(Error::InvalidState);
                };
                let (labels, work, budget) = cursor.finish(now).map_err(Error::Labels)?;
                self.labels = Some(labels);
                self.owner = Owner::Budgets(work, budget, scratch);
                Ok(Status::Complete)
            }
            Owner::Budgets(..) | Owner::Retired => Err(Error::InvalidState),
        }
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<
        (
            View<'w>,
            &'w mut Meter,
            &'w mut HeaderBudget,
            &'w mut Scratch,
        ),
        Error,
    > {
        self.check_deadline(now)?;
        if !self.is_complete() {
            return Err(Error::InvalidState);
        }
        let Owner::Budgets(work, budget, scratch) = self.owner else {
            return Err(Error::InvalidState);
        };
        Ok((
            View {
                headers: self.headers.ok_or(Error::InvalidState)?,
                labels: self.labels.ok_or(Error::InvalidState)?,
            },
            work,
            budget,
            scratch,
        ))
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 6 * 1024
);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::{
        admission::work::{Charge, Stop},
        header_select::SourceEnd,
        mime_metadata::Context,
        ports::Deadline,
    };
    const SOURCE: &[u8] = concat!(
        "Content-Type: TeXT/HTmL;charset=UtF-8\r\n",
        "Content-Disposition: INLINE;filename*=utf-8''e%CC%81\r\n",
        "Content-ID: bad\r\nContent-ID: (x)<A@B>\r\nContent-ID: <later@id>\r\n",
        "Content-Language: bad_\r\nContent-Language: EN-us, EN-us\r\nContent-Language: fr\r\n",
        "\r\nContent-ID: <body@id>\r\n"
    )
    .as_bytes();
    struct Storage {
        heads: [u8; 128],
        charset: [u8; 32],
        name: [u8; 128],
        id: [u8; 256],
        lang: [u8; 256],
    }
    impl Storage {
        fn new() -> Self {
            Self {
                heads: [0xa5; 128],
                charset: [0xa5; 32],
                name: [0xa5; 128],
                id: [0xa5; 256],
                lang: [0xa5; 256],
            }
        }
        fn backing(&mut self, id: usize, lang: usize) -> Backing<'_> {
            Backing {
                headers: super::super::Backing {
                    heads: &mut self.heads,
                    charset: &mut self.charset,
                    filename: &mut self.name,
                },
                labels: labels::Backing {
                    content_id: &mut self.id[..id],
                    content_language: &mut self.lang[..lang],
                },
            }
        }
    }
    fn meter() -> Meter {
        Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: 100_000_000,
                records: 100_000_000,
                output_bytes: 100_000_000,
                ..Charge::default()
            },
        )
    }
    fn entity(source: &[u8], base: u64) -> super::super::Entity<'_> {
        super::super::Entity {
            source,
            base,
            source_end: SourceEnd::Eof,
            header_limit: 1_000_000,
            context: Context::Normal,
        }
    }
    fn drain(cursor: &mut Cursor<'_, '_>) -> Result<usize, Error> {
        for turn in 1..200_000 {
            if cursor.poll(Tick(1))? == Status::Complete {
                return Ok(turn);
            }
            assert!(cursor.value().is_none());
            assert!(!cursor.is_complete());
        }
        panic!("part label JSON did not complete")
    }
    #[test]
    fn selected_values_same_source_bounds_and_original_owner_reuse() {
        for base in [0, 37, u64::MAX - SOURCE.len() as u64] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut storage = Storage::new();
            let identity = (
                std::ptr::from_ref(&work),
                std::ptr::from_ref(&budget),
                std::ptr::from_ref(&scratch),
            );
            let mut cursor = Cursor::new(
                entity(SOURCE, base),
                storage.backing(5, 17),
                &mut work,
                &mut budget,
                &mut scratch,
            )
            .unwrap();
            drain(&mut cursor).unwrap();
            let view = cursor.value().unwrap();
            assert_eq!(view.headers.content_type, b"text/html");
            assert_eq!(view.headers.disposition, Some(b"inline".as_slice()));
            assert_eq!(view.headers.charset, Some(b"UtF-8".as_slice()));
            assert_eq!(view.headers.filename, Some("é".as_bytes()));
            assert_eq!(view.labels.content_id, Some(b"\"A@B\"".as_slice()));
            assert_eq!(
                view.labels.content_language,
                Some(b"[\"EN-us\",\"EN-us\"]".as_slice())
            );
            let field = view.headers.content_id_field.unwrap();
            let raw = cursor.field(&view.headers, field).unwrap();
            assert_eq!(raw, b" (x)<A@B>");
            assert_eq!(
                raw.as_ptr(),
                SOURCE[field.value_start.checked_sub(base).unwrap() as usize..].as_ptr()
            );
            for bad in [
                Field {
                    value_end: base + view.headers.header_bytes + 1,
                    ..field
                },
                Field {
                    value_start: field.value_end + 1,
                    ..field
                },
                Field {
                    name_end: field.value_start + 1,
                    ..field
                },
            ] {
                assert_eq!(cursor.field(&view.headers, bad), Err(Error::InvalidState));
            }
            if let Some(before_base) = base.checked_sub(1) {
                assert_eq!(
                    cursor.field(
                        &view.headers,
                        Field {
                            name_start: before_base,
                            ..field
                        }
                    ),
                    Err(Error::InvalidState)
                );
            }
            assert_eq!(
                cursor.field(
                    &view.headers,
                    Field {
                        name_start: field.name_end + 1,
                        ..field
                    }
                ),
                Err(Error::InvalidState)
            );
            let bad_end = super::super::View {
                body_start: base + view.headers.header_bytes - 1,
                ..view.headers
            };
            assert_eq!(cursor.field(&bad_end, field), Err(Error::InvalidState));
            if base != 0 {
                let overflow = super::super::View {
                    header_bytes: u64::MAX,
                    ..view.headers
                };
                assert_eq!(cursor.field(&overflow, field), Err(Error::InvalidState));
            }
            if base.checked_add(SOURCE.len() as u64 + 1).is_some() {
                let beyond = super::super::View {
                    header_bytes: SOURCE.len() as u64 + 1,
                    body_start: base + SOURCE.len() as u64 + 1,
                    ..view.headers
                };
                assert_eq!(
                    cursor.field(
                        &beyond,
                        Field {
                            value_end: beyond.body_start,
                            ..field
                        }
                    ),
                    Err(Error::InvalidState)
                );
            }
            assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
            let (_, work, budget, scratch) = cursor.finish(Tick(1)).unwrap();
            assert_eq!(
                (
                    std::ptr::from_ref(&*work),
                    std::ptr::from_ref(&*budget),
                    std::ptr::from_ref(&*scratch)
                ),
                identity
            );
            let mut next = Storage::new();
            let backing = next.backing(0, 0);
            let mut next = super::super::Cursor::new(
                entity(b"\r\n", 0),
                backing.headers,
                work,
                budget,
                scratch,
            )
            .unwrap();
            for _ in 0..10000 {
                if next.poll(Tick(1)).unwrap() == Status::Complete {
                    break;
                }
            }
            let (_, work, budget, scratch) = next.finish(Tick(1)).unwrap();
            assert_eq!(
                (
                    std::ptr::from_ref(&*work),
                    std::ptr::from_ref(&*budget),
                    std::ptr::from_ref(&*scratch)
                ),
                identity
            );
            assert!(storage.id[5..].iter().all(|b| *b == 0xa5));
            assert!(storage.lang[17..].iter().all(|b| *b == 0xa5));
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut storage = Storage::new();
        let mut cursor = Cursor::new(
            entity(b"\r\nContent-ID: <body@id>\r\n", 3),
            storage.backing(0, 0),
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let (view, _, _, _) = cursor.finish(Tick(1)).unwrap();
        assert_eq!(view.headers.body_start, 5);
        assert_eq!(view.labels.content_id, None);
        assert_eq!(view.labels.content_language, None);
        assert!(storage.id.iter().all(|b| *b == 0xa5));
        assert!(storage.lang.iter().all(|b| *b == 0xa5));
    }
    #[test]
    fn every_label_capacity_cut_hides_completed_headers_and_pair() {
        for kind in 0..2 {
            for cap in 0..if kind == 0 { 5 } else { 17 } {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let mut scratch = Scratch::new();
                let mut storage = Storage::new();
                let mut cursor = Cursor::new(
                    entity(SOURCE, 0),
                    storage.backing(
                        if kind == 0 { cap } else { 5 },
                        if kind == 1 { cap } else { 17 },
                    ),
                    &mut work,
                    &mut budget,
                    &mut scratch,
                )
                .unwrap();
                let error = Error::Labels(labels::Error::OutputCapacity);
                assert_eq!(drain(&mut cursor), Err(error));
                assert!(cursor.value().is_none());
                assert!(!cursor.is_complete());
                assert!(cursor.headers.is_none());
                assert!(cursor.labels.is_none());
                assert!(matches!(cursor.owner, Owner::Retired));
                assert_eq!(cursor.check_deadline(Tick(100)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                assert_eq!(work.stopped(), None);
            }
        }
    }

    #[derive(Debug, Eq, PartialEq)]
    struct Costs {
        work: Charge,
        bytes: u64,
        steps: u64,
    }
    fn costs(work: &Meter, budget: &HeaderBudget) -> Costs {
        Costs {
            work: work.remaining(),
            bytes: budget.source_bytes_remaining(),
            steps: budget.steps_remaining(),
        }
    }
    fn standalone_prefix(cut: usize) -> (Costs, bool) {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut storage = Storage::new();
        let mut complete = false;
        {
            let backing = storage.backing(5, 17);
            let mut cursor = super::super::Cursor::new(
                entity(SOURCE, 37),
                backing.headers,
                &mut work,
                &mut budget,
                &mut scratch,
            )
            .unwrap();
            let mut header_turns = 0;
            for _ in 0..cut {
                header_turns += 1;
                if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                    break;
                }
            }
            if let Ok((view, work, budget, _)) = cursor.finish(Tick(1)) {
                let values = labels::Values {
                    content_id: view.content_id_field.map(|field| {
                        td_header::resident::slice(SOURCE, 37, field.value_start..field.value_end)
                            .unwrap()
                    }),
                    content_language: view.content_language_field.map(|field| {
                        td_header::resident::slice(SOURCE, 37, field.value_start..field.value_end)
                            .unwrap()
                    }),
                };
                let mut cursor = labels::Cursor::new(values, backing.labels, work, budget);
                for _ in header_turns..cut {
                    if cursor.poll(Tick(1)).unwrap() == labels::Status::Complete {
                        complete = true;
                        break;
                    }
                }
                let result = cursor.finish(Tick(1));
                assert_eq!(result.is_ok(), complete);
            }
        }
        (costs(&work, &budget), complete)
    }
    #[test]
    fn every_turn_matches_standalone_header_and_label_costs() {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut storage = Storage::new();
        let mut cursor = Cursor::new(
            entity(SOURCE, 37),
            storage.backing(5, 17),
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        let turns = drain(&mut cursor).unwrap();
        cursor.finish(Tick(1)).unwrap();
        for cut in 0..=turns {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut storage = Storage::new();
            {
                let mut cursor = Cursor::new(
                    entity(SOURCE, 37),
                    storage.backing(5, 17),
                    &mut work,
                    &mut budget,
                    &mut scratch,
                )
                .unwrap();
                for _ in 0..cut {
                    cursor.poll(Tick(1)).unwrap();
                }
                assert_eq!(cursor.is_complete(), cut == turns);
                assert_eq!(cursor.finish(Tick(1)).is_ok(), cut == turns);
            }
            let (expected, complete) = standalone_prefix(cut);
            assert_eq!(complete, cut == turns, "completion cut {cut}");
            assert_eq!(costs(&work, &budget), expected, "cost cut {cut}");
        }
    }

    fn limited(kind: usize, cap: u64) -> (Meter, HeaderBudget) {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        if kind < 2 {
            let bytes = if kind == 0 {
                budget.source_bytes_remaining() - cap
            } else {
                0
            };
            let steps = if kind == 1 {
                budget.steps_remaining() - cap
            } else {
                0
            };
            budget
                .charge(&mut meter(), Tick(1), bytes, steps, &mut 0)
                .unwrap();
        } else {
            let mut grants = work.remaining();
            match kind {
                2 => grants.io_bytes = cap,
                3 => grants.records = cap,
                4 => grants.output_bytes = cap,
                _ => panic!("bad resource"),
            };
            work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), grants);
        }
        (work, budget)
    }
    fn standalone_refusal(kind: usize, cap: u64) -> Option<Error> {
        let (mut work, mut budget) = limited(kind, cap);
        let mut scratch = Scratch::new();
        let mut storage = Storage::new();
        let backing = storage.backing(5, 17);
        let mut cursor = super::super::Cursor::new(
            entity(SOURCE, 0),
            backing.headers,
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        let mut complete = false;
        for _ in 0..200_000 {
            match cursor.poll(Tick(1)) {
                Ok(Status::Complete) => {
                    complete = true;
                    break;
                }
                Ok(Status::Yield) => {}
                Err(error) => return Some(Error::Headers(error)),
            }
        }
        assert!(complete);
        let (view, work, budget, _) = match cursor.finish(Tick(1)) {
            Ok(value) => value,
            Err(error) => return Some(Error::Headers(error)),
        };
        let values = labels::Values {
            content_id: view.content_id_field.map(|field| {
                td_header::resident::slice(SOURCE, 0, field.value_start..field.value_end).unwrap()
            }),
            content_language: view.content_language_field.map(|field| {
                td_header::resident::slice(SOURCE, 0, field.value_start..field.value_end).unwrap()
            }),
        };
        let mut cursor = labels::Cursor::new(values, backing.labels, work, budget);
        let mut complete = false;
        for _ in 0..200_000 {
            match cursor.poll(Tick(1)) {
                Ok(labels::Status::Complete) => {
                    complete = true;
                    break;
                }
                Ok(labels::Status::Yield) => {}
                Err(error) => return Some(Error::Labels(error)),
            }
        }
        assert!(complete);
        cursor.finish(Tick(1)).err().map(Error::Labels)
    }

    #[test]
    fn every_original_resource_cut_and_exact_grants() {
        let mut work = meter();
        let initial = work.remaining();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut storage = Storage::new();
        let mut cursor = Cursor::new(
            entity(SOURCE, 0),
            storage.backing(5, 17),
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        cursor.finish(Tick(1)).unwrap();
        let used = [
            HeaderBudget::new().source_bytes_remaining() - budget.source_bytes_remaining(),
            HeaderBudget::new().steps_remaining() - budget.steps_remaining(),
            initial.io_bytes - work.remaining().io_bytes,
            initial.records - work.remaining().records,
            initial.output_bytes - work.remaining().output_bytes,
        ];
        assert!(used.iter().all(|cost| *cost > 0));
        for (kind, cost) in used.into_iter().enumerate() {
            for cap in 0..cost {
                let (mut work, mut budget) = limited(kind, cap);
                let mut scratch = Scratch::new();
                let mut storage = Storage::new();
                let mut cursor = Cursor::new(
                    entity(SOURCE, 0),
                    storage.backing(5, 17),
                    &mut work,
                    &mut budget,
                    &mut scratch,
                )
                .unwrap();
                let error = drain(&mut cursor).unwrap_err();
                assert_eq!(Some(error), standalone_refusal(kind, cap));
                assert!(matches!(cursor.owner, Owner::Retired));
                assert!(cursor.value().is_none());
                assert!(!cursor.is_complete());
                assert!(cursor.headers.is_none());
                assert!(cursor.labels.is_none());
                assert_eq!(cursor.poll(Tick(100)), Err(error));
                assert_eq!(cursor.finish(Tick(1)).err(), Some(error));
                if kind < 2 {
                    assert_eq!(work.stopped(), None);
                    let mut next = crate::mime_language::Cursor::new(b"fr", &mut work, &mut budget);
                    assert_eq!(
                        next.check_deadline(Tick(1)),
                        Err(crate::mime_language::Error::InterpretationLimit)
                    );
                } else {
                    assert_eq!(
                        work.stopped(),
                        Some(match kind {
                            2 => Stop::IoBytes,
                            3 => Stop::Records,
                            4 => Stop::OutputBytes,
                            _ => panic!("bad resource"),
                        })
                    );
                }
            }
        }
        let mut work = Meter::new(
            Deadline::after(Tick(0), 100).unwrap(),
            Charge {
                io_bytes: used[2],
                records: used[3],
                output_bytes: used[4],
                ..Charge::default()
            },
        );
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut storage = Storage::new();
        budget
            .charge(
                &mut meter(),
                Tick(1),
                budget.source_bytes_remaining() - used[0],
                budget.steps_remaining() - used[1],
                &mut 0,
            )
            .unwrap();
        let mut cursor = Cursor::new(
            entity(SOURCE, 0),
            storage.backing(5, 17),
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        cursor.finish(Tick(1)).unwrap();
        assert_eq!(budget.source_bytes_remaining(), 0);
        assert_eq!(budget.steps_remaining(), 0);
        assert_eq!(work.remaining(), Charge::default());
        assert_eq!(work.stopped(), None);
    }

    #[test]
    fn fresh_admission_and_premature_finish_at_every_composition_cut() {
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut scratch = Scratch::new();
        let mut storage = Storage::new();
        let mut cursor = Cursor::new(
            entity(SOURCE, 0),
            storage.backing(5, 17),
            &mut work,
            &mut budget,
            &mut scratch,
        )
        .unwrap();
        let turns = drain(&mut cursor).unwrap();
        cursor.finish(Tick(1)).unwrap();
        for cut in 0..=turns {
            for trial in 0..3 {
                let mut work = meter();
                let mut budget = HeaderBudget::new();
                let mut scratch = Scratch::new();
                let mut storage = Storage::new();
                let mut cursor = Cursor::new(
                    entity(SOURCE, 0),
                    storage.backing(5, 17),
                    &mut work,
                    &mut budget,
                    &mut scratch,
                )
                .unwrap();
                for _ in 0..cut {
                    cursor.poll(Tick(1)).unwrap();
                }
                if trial == 0 {
                    assert!(cursor.check_deadline(Tick(100)).is_err());
                    assert!(cursor.value().is_none());
                    assert!(!cursor.is_complete());
                    assert!(cursor.headers.is_none());
                    assert!(cursor.labels.is_none());
                    assert!(cursor.finish(Tick(1)).is_err());
                    assert_eq!(work.stopped(), Some(Stop::Deadline));
                } else if trial == 1 {
                    assert!(cursor.finish(Tick(100)).is_err());
                    assert_eq!(work.stopped(), Some(Stop::Deadline));
                } else if cut == turns {
                    assert!(cursor.finish(Tick(1)).is_ok());
                } else {
                    assert_eq!(cursor.finish(Tick(1)).err(), Some(Error::InvalidState));
                    assert_eq!(work.stopped(), None);
                }
            }
        }
    }
}
