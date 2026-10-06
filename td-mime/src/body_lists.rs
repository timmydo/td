//! Iterative body-list selection over caller-supplied complete preorder evidence.
use crate::{
    nfc::{self, HeaderBudget},
    part_headers::View as Headers,
    structure::Limits,
    time::Tick,
    work::{Charge, Meter, Stop},
};
const TEXT: u8 = 1;
const HTML: u8 = 2;
const FRAMES: usize = 65;
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Media {
    #[default]
    Other,
    Plain,
    Html,
    InlineMedia,
    Multipart,
    Alternative,
    Related,
}
impl Media {
    const fn multipart(self) -> bool {
        matches!(self, Self::Multipart | Self::Alternative | Self::Related)
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Disposition {
    #[default]
    Other,
    Inline,
    Attachment,
}
/// Passive selection facts, never a source or completion capability.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Class {
    pub media: Media,
    pub disposition: Disposition,
    /// Empty selected filenames are present metadata but false for body selection.
    pub named: bool,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Node {
    pub parent: u16,
    pub depth: u8,
    pub class: Class,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidLimits,
    PartLimit,
    DepthLimit,
    InvalidTree,
    InvalidState,
    OutputCapacity,
    Work(Stop),
    InterpretationLimit,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidLimits => "invalid MIME body-list limits",
            Self::PartLimit => "MIME body-list part limit",
            Self::DepthLimit => "MIME body-list depth limit",
            Self::InvalidTree => "invalid MIME body-list preorder tree",
            Self::InvalidState => "invalid MIME body-list state",
            Self::OutputCapacity => "MIME body-list capacity",
            Self::Work(_) => "MIME body-list work refusal",
            Self::InterpretationLimit => "MIME body-list interpretation refusal",
        })
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        if let Self::Work(stop) = self {
            Some(stop)
        } else {
            None
        }
    }
}
impl From<nfc::Error> for Error {
    fn from(error: nfc::Error) -> Self {
        match error {
            nfc::Error::Work(stop) => Self::Work(stop),
            nfc::Error::InterpretationLimit => Self::InterpretationLimit,
            _ => Self::InvalidState,
        }
    }
}
impl Class {
    /// Fixed comparisons of retained heads; caller supplies healthy original owners.
    pub fn from_headers(
        headers: Headers<'_>,
        now: Tick,
        work: &mut Meter,
        budget: &mut HeaderBudget,
    ) -> Result<Self, Error> {
        // All fixed token/prefix comparisons together visit fewer than 64 bytes.
        budget.charge_local(work, now, 64, 64, &mut 0)?;
        work.charge(
            now,
            Charge {
                output_bytes: std::mem::size_of::<Self>() as u64,
                ..Charge::default()
            },
        )
        .map_err(Error::Work)?;
        let typ = headers.content_type;
        let media = if typ == b"text/plain" {
            Media::Plain
        } else if typ == b"text/html" {
            Media::Html
        } else if typ.starts_with(b"image/")
            || typ.starts_with(b"audio/")
            || typ.starts_with(b"video/")
        {
            Media::InlineMedia
        } else if typ == b"multipart/alternative" {
            Media::Alternative
        } else if typ == b"multipart/related" {
            Media::Related
        } else if typ.starts_with(b"multipart/") {
            Media::Multipart
        } else {
            Media::Other
        };
        let disposition = match headers.disposition {
            Some(b"inline") => Disposition::Inline,
            Some(b"attachment") => Disposition::Attachment,
            _ => Disposition::Other,
        };
        Ok(Self {
            media,
            disposition,
            named: headers.filename.is_some_and(|value| !value.is_empty()),
        })
    }
}
/// The caller admits retained list backing separately and flags beside nodes.
pub struct Backing<'w> {
    pub text: &'w mut [u16],
    pub html: &'w mut [u16],
    pub attachments: &'w mut [u16],
    pub membership: &'w mut [u8],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct View<'w> {
    pub text: &'w [u16],
    pub html: &'w [u16],
    pub attachments: &'w [u16],
    pub has_attachment: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    Complete,
}
#[derive(Clone, Copy, Default)]
struct Frame {
    ordinal: u16,
    depth: u8,
    kind: Media,
    alternative: bool,
    mask: u8,
    children: u16,
    text_start: usize,
    html_start: usize,
}
#[derive(Clone, Copy)]
enum Phase {
    Tree,
    Copy {
        to_text: bool,
        position: usize,
        end: usize,
    },
    Pop,
    Attachments,
    Complete,
}
/// Original job and header owners remain exclusive; outputs appear only on success.
/// Nodes must correspond to the caller's authorized completed traversal/metadata.
/// ```compile_fail,E0277
/// fn copied<T: Copy>() {} copied::<td_mime::body_lists::Cursor<'_, '_>>();
/// ```
/// ```compile_fail,E0277
/// fn cloned<T: Clone>() {} cloned::<td_mime::body_lists::Cursor<'_, '_>>();
/// ```
pub struct Cursor<'a, 'w> {
    nodes: &'a [Node],
    work: &'w mut Meter,
    budget: &'w mut HeaderBudget,
    output: Backing<'w>,
    frames: [Frame; FRAMES],
    active: usize,
    index: usize,
    text_len: usize,
    html_len: usize,
    attachment_len: usize,
    has_attachment: bool,
    max_depth: usize,
    phase: Phase,
    failure: Option<Error>,
}
impl<'a, 'w> Cursor<'a, 'w> {
    pub fn new(
        nodes: &'a [Node],
        limits: impl Into<Limits>,
        output: Backing<'w>,
        work: &'w mut Meter,
        budget: &'w mut HeaderBudget,
    ) -> Result<Self, Error> {
        let limits = limits.into();
        if !(1..=64).contains(&limits.mime_depth)
            || !(1..=4096).contains(&limits.mime_parts)
            || limits.mime_depth > limits.mime_parts
        {
            return Err(Error::InvalidLimits);
        }
        if nodes.is_empty() {
            return Err(Error::InvalidTree);
        }
        if nodes.len() > limits.mime_parts {
            return Err(Error::PartLimit);
        }
        let mut frames = [Frame::default(); FRAMES];
        let root = frames.first_mut().ok_or(Error::InvalidState)?;
        root.mask = TEXT | HTML;
        root.kind = Media::Multipart;
        Ok(Self {
            nodes,
            work,
            budget,
            output,
            frames,
            active: 1,
            index: 0,
            text_len: 0,
            html_len: 0,
            attachment_len: 0,
            has_attachment: false,
            max_depth: limits.mime_depth,
            phase: Phase::Tree,
            failure: None,
        })
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self
            .budget
            .charge_local(self.work, now, 0, 0, &mut 0)
            .map_err(Error::from);
        self.outcome(result)
    }
    pub fn value(&self) -> Option<View<'_>> {
        if self.failure.is_some() || !matches!(self.phase, Phase::Complete) {
            return None;
        }
        Some(View {
            text: self.output.text.get(..self.text_len)?,
            html: self.output.html.get(..self.html_len)?,
            attachments: self.output.attachments.get(..self.attachment_len)?,
            has_attachment: self.has_attachment,
        })
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<(View<'w>, &'w mut Meter, &'w mut HeaderBudget), Error> {
        self.check_deadline(now)?;
        if !matches!(self.phase, Phase::Complete) {
            return Err(Error::InvalidState);
        }
        Ok((
            View {
                text: self
                    .output
                    .text
                    .get(..self.text_len)
                    .ok_or(Error::InvalidState)?,
                html: self
                    .output
                    .html
                    .get(..self.html_len)
                    .ok_or(Error::InvalidState)?,
                attachments: self
                    .output
                    .attachments
                    .get(..self.attachment_len)
                    .ok_or(Error::InvalidState)?,
                has_attachment: self.has_attachment,
            },
            self.work,
            self.budget,
        ))
    }
    pub fn poll(&mut self, now: Tick) -> Result<Status, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if matches!(self.phase, Phase::Complete) {
            return Ok(Status::Complete);
        }
        let result = self.step(now);
        self.outcome(result)
    }
    fn frame_index(&self) -> Result<usize, Error> {
        self.active.checked_sub(1).ok_or(Error::InvalidState)
    }
    fn append(&mut self, now: Tick, ordinal: u16, text: bool) -> Result<(), Error> {
        let index = usize::from(ordinal.checked_sub(1).ok_or(Error::InvalidState)?);
        let flag = self
            .output
            .membership
            .get_mut(index)
            .ok_or(Error::OutputCapacity)?;
        let mask = if text { TEXT } else { HTML };
        self.work
            .charge(
                now,
                Charge {
                    io_bytes: 1,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work)?;
        if *flag & mask != 0 {
            return Err(Error::InvalidState);
        }
        let (output, len) = if text {
            (&mut self.output.text, &mut self.text_len)
        } else {
            (&mut self.output.html, &mut self.html_len)
        };
        let cell = output.get_mut(*len).ok_or(Error::OutputCapacity)?;
        self.work
            .charge(
                now,
                Charge {
                    output_bytes: 3,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work)?;
        *cell = ordinal;
        *len = len.checked_add(1).ok_or(Error::InvalidState)?;
        *flag |= mask;
        Ok(())
    }
    fn close_frame(&mut self) -> Result<(), Error> {
        let frame = *self
            .frames
            .get(self.frame_index()?)
            .ok_or(Error::InvalidState)?;
        self.phase = if frame.kind == Media::Alternative && frame.mask == TEXT | HTML {
            if self.text_len == frame.text_start && self.html_len != frame.html_start {
                Phase::Copy {
                    to_text: true,
                    position: frame.html_start,
                    end: self.html_len,
                }
            } else if self.html_len == frame.html_start && self.text_len != frame.text_start {
                Phase::Copy {
                    to_text: false,
                    position: frame.text_start,
                    end: self.text_len,
                }
            } else {
                Phase::Pop
            }
        } else {
            Phase::Pop
        };
        Ok(())
    }
    fn step(&mut self, now: Tick) -> Result<Status, Error> {
        self.check_deadline(now)?;
        self.work
            .charge(
                now,
                Charge {
                    records: 1,
                    ..Charge::default()
                },
            )
            .map_err(Error::Work)?;
        match self.phase {
            Phase::Tree => {
                let frame_index = self.frame_index()?;
                let frame = *self.frames.get(frame_index).ok_or(Error::InvalidState)?;
                let Some(node) = self.nodes.get(self.index) else {
                    self.close_frame()?;
                    return Ok(Status::Yield);
                };
                self.work
                    .charge(
                        now,
                        Charge {
                            io_bytes: std::mem::size_of::<Node>() as u64,
                            ..Charge::default()
                        },
                    )
                    .map_err(Error::Work)?;
                let node = *node;
                if node.depth == 0 {
                    return Err(Error::InvalidTree);
                }
                if usize::from(node.depth) > self.max_depth {
                    return Err(Error::DepthLimit);
                }
                if node.depth <= frame.depth {
                    self.close_frame()?;
                    return Ok(Status::Yield);
                }
                if node.depth != frame.depth.checked_add(1).ok_or(Error::InvalidTree)?
                    || node.parent != frame.ordinal
                    || (self.index != 0 && node.parent == 0)
                {
                    return Err(Error::InvalidTree);
                }
                let ordinal = u16::try_from(self.index.checked_add(1).ok_or(Error::PartLimit)?)
                    .map_err(|_| Error::PartLimit)?;
                let flag = self
                    .output
                    .membership
                    .get_mut(self.index)
                    .ok_or(Error::OutputCapacity)?;
                self.work
                    .charge(
                        now,
                        Charge {
                            output_bytes: 1,
                            ..Charge::default()
                        },
                    )
                    .map_err(Error::Work)?;
                *flag = 0;
                self.frames
                    .get_mut(frame_index)
                    .ok_or(Error::InvalidState)?
                    .children = frame.children.checked_add(1).ok_or(Error::PartLimit)?;
                self.index = self.index.checked_add(1).ok_or(Error::InvalidState)?;
                let class = node.class;
                if class.media.multipart() {
                    let target = self.frames.get_mut(self.active).ok_or(Error::DepthLimit)?;
                    *target = Frame {
                        ordinal,
                        depth: node.depth,
                        kind: class.media,
                        alternative: frame.alternative || class.media == Media::Alternative,
                        mask: frame.mask,
                        children: 0,
                        text_start: self.text_len,
                        html_start: self.html_len,
                    };
                    self.active = self.active.checked_add(1).ok_or(Error::DepthLimit)?;
                } else {
                    let inline = class.disposition != Disposition::Attachment
                        && matches!(class.media, Media::Plain | Media::Html | Media::InlineMedia)
                        && (frame.children == 0
                            || (frame.kind != Media::Related
                                && (class.media == Media::InlineMedia || !class.named)));
                    if inline {
                        let mut mask = frame.mask;
                        if frame.kind == Media::Alternative {
                            mask &= match class.media {
                                Media::Plain => TEXT,
                                Media::Html => HTML,
                                _ => 0,
                            };
                        } else if frame.alternative {
                            if class.media == Media::Plain {
                                mask &= !HTML;
                            }
                            if class.media == Media::Html {
                                mask &= !TEXT;
                            }
                            self.frames
                                .get_mut(frame_index)
                                .ok_or(Error::InvalidState)?
                                .mask = mask;
                        }
                        if mask & TEXT != 0 {
                            self.append(now, ordinal, true)?;
                        }
                        if mask & HTML != 0 {
                            self.append(now, ordinal, false)?;
                        }
                    }
                }
            }
            Phase::Copy {
                to_text,
                position,
                end,
            } => {
                if position == end {
                    self.phase = Phase::Pop;
                    return Ok(Status::Yield);
                }
                let source = if to_text {
                    &self.output.html
                } else {
                    &self.output.text
                };
                self.work
                    .charge(
                        now,
                        Charge {
                            io_bytes: 2,
                            ..Charge::default()
                        },
                    )
                    .map_err(Error::Work)?;
                let ordinal = *source.get(position).ok_or(Error::InvalidState)?;
                self.append(now, ordinal, to_text)?;
                self.phase = Phase::Copy {
                    to_text,
                    position: position.checked_add(1).ok_or(Error::InvalidState)?,
                    end,
                };
            }
            Phase::Pop => {
                self.active = self.active.checked_sub(1).ok_or(Error::InvalidState)?;
                if self.active == 0 {
                    if self.index != self.nodes.len() {
                        return Err(Error::InvalidState);
                    }
                    self.index = 0;
                    self.phase = Phase::Attachments;
                } else {
                    self.phase = Phase::Tree;
                }
            }
            Phase::Attachments => {
                let Some(node) = self.nodes.get(self.index) else {
                    self.phase = Phase::Complete;
                    return Ok(Status::Complete);
                };
                self.work
                    .charge(
                        now,
                        Charge {
                            io_bytes: (std::mem::size_of::<Node>() + 1) as u64,
                            ..Charge::default()
                        },
                    )
                    .map_err(Error::Work)?;
                let membership = *self
                    .output
                    .membership
                    .get(self.index)
                    .ok_or(Error::InvalidState)?;
                if !node.class.media.multipart()
                    && (membership == 0
                        || (node.class.media == Media::InlineMedia && membership != TEXT | HTML))
                {
                    let ordinal = u16::try_from(self.index.checked_add(1).ok_or(Error::PartLimit)?)
                        .map_err(|_| Error::PartLimit)?;
                    let cell = self
                        .output
                        .attachments
                        .get_mut(self.attachment_len)
                        .ok_or(Error::OutputCapacity)?;
                    self.work
                        .charge(
                            now,
                            Charge {
                                output_bytes: 2,
                                ..Charge::default()
                            },
                        )
                        .map_err(Error::Work)?;
                    *cell = ordinal;
                    self.attachment_len =
                        self.attachment_len.checked_add(1).ok_or(Error::PartLimit)?;
                    self.has_attachment |= node.class.disposition != Disposition::Inline;
                }
                self.index = self.index.checked_add(1).ok_or(Error::InvalidState)?;
            }
            Phase::Complete => return Ok(Status::Complete),
        }
        Ok(Status::Yield)
    }
}
const _: () = assert!(
    std::mem::size_of::<Cursor<'_, '_>>() + std::mem::size_of::<HeaderBudget>() <= 16 * 1024
);
const _: () = assert!(
    std::mem::size_of::<crate::structure::Part>()
        + std::mem::size_of::<Node>()
        + std::mem::size_of::<u8>()
        <= 64
);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::time::Deadline;
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
    fn node(parent: u16, depth: u8, media: Media) -> Node {
        Node {
            parent,
            depth,
            class: Class {
                media,
                ..Class::default()
            },
        }
    }
    fn run(nodes: &[Node]) -> (Vec<u16>, Vec<u16>, Vec<u16>, bool) {
        let mut text = vec![0; nodes.len()];
        let mut html = vec![0; nodes.len()];
        let mut attachments = vec![0; nodes.len()];
        let mut flags = vec![0; nodes.len()];
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let limits = Limits {
            mime_depth: 64,
            mime_parts: 4096,
            ..Limits::default()
        };
        let mut cursor = Cursor::new(
            nodes,
            limits,
            Backing {
                text: &mut text,
                html: &mut html,
                attachments: &mut attachments,
                membership: &mut flags,
            },
            &mut work,
            &mut budget,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let value = cursor.finish(Tick(1)).unwrap().0;
        (
            value.text.to_vec(),
            value.html.to_vec(),
            value.attachments.to_vec(),
            value.has_attachment,
        )
    }
    fn drain(cursor: &mut Cursor<'_, '_>) -> Result<(), Error> {
        for _ in 0..100_000 {
            let before = cursor.work.remaining();
            let bytes = cursor.budget.source_bytes_remaining();
            let steps = cursor.budget.steps_remaining();
            let status = cursor.poll(Tick(1));
            let after = cursor.work.remaining();
            assert!(before.io_bytes - after.io_bytes <= 8);
            assert!(before.records - after.records <= 1);
            assert!(before.output_bytes - after.output_bytes <= 7);
            assert_eq!(bytes, cursor.budget.source_bytes_remaining());
            assert_eq!(steps, cursor.budget.steps_remaining());
            if status? == Status::Complete {
                return Ok(());
            }
            assert!(cursor.value().is_none());
        }
        panic!("body-list cursor did not finish")
    }
    #[test]
    fn rfc_a_through_k_literal_membership() {
        let mut nodes = vec![
            node(0, 1, Media::Multipart),
            node(1, 2, Media::Plain),
            node(1, 2, Media::Multipart),
            node(3, 3, Media::Alternative),
            node(4, 4, Media::Multipart),
            node(5, 5, Media::Plain),
            node(5, 5, Media::InlineMedia),
            node(5, 5, Media::Plain),
            node(4, 4, Media::Related),
            node(9, 5, Media::Html),
            node(9, 5, Media::InlineMedia),
            node(3, 3, Media::InlineMedia),
            node(3, 3, Media::Other),
            node(3, 3, Media::Other),
            node(1, 2, Media::Plain),
        ];
        for ordinal in [2, 6, 7, 8, 15] {
            nodes[ordinal - 1].class.disposition = Disposition::Inline;
        }
        nodes[11].class.disposition = Disposition::Attachment;
        assert_eq!(
            run(&nodes),
            (
                vec![2, 6, 7, 8, 15],
                vec![2, 10, 15],
                vec![7, 11, 12, 13, 14],
                true
            )
        );
    }
    #[test]
    fn fallback_related_named_and_nested_disabled_channels() {
        for (media, text, html, attachments) in [
            (Media::Plain, vec![2], vec![2], vec![]),
            (Media::Html, vec![2], vec![2], vec![]),
            (Media::InlineMedia, vec![], vec![], vec![2]),
            (Media::Other, vec![], vec![], vec![2]),
        ] {
            assert_eq!(
                run(&[node(0, 1, Media::Alternative), node(1, 2, media)]),
                (
                    text,
                    html,
                    attachments,
                    matches!(media, Media::InlineMedia | Media::Other)
                )
            );
        }
        let mut nodes = [
            node(0, 1, Media::Related),
            node(1, 2, Media::Plain),
            node(1, 2, Media::Plain),
            node(1, 2, Media::InlineMedia),
        ];
        assert_eq!(run(&nodes), (vec![2], vec![2], vec![3, 4], true));
        nodes[0].class.media = Media::Multipart;
        nodes[2].class.named = true;
        assert_eq!(run(&nodes), (vec![2, 4], vec![2, 4], vec![3], true));
        nodes[1].class.named = true;
        nodes[2].class.named = false;
        nodes[3].class.named = true;
        assert_eq!(run(&nodes), (vec![2, 3, 4], vec![2, 3, 4], vec![], false));
        nodes[2].class.disposition = Disposition::Attachment;
        assert_eq!(run(&nodes), (vec![2, 4], vec![2, 4], vec![3], true));
        nodes[2].class.media = Media::Other;
        nodes[2].class.disposition = Disposition::Inline;
        assert_eq!(run(&nodes), (vec![2, 4], vec![2, 4], vec![3], false));
        let fallback_media = [
            node(0, 1, Media::Alternative),
            node(1, 2, Media::Multipart),
            node(2, 3, Media::Plain),
            node(2, 3, Media::InlineMedia),
        ];
        // Final fallback places the image in both lists, so it is no attachment.
        assert_eq!(
            run(&fallback_media),
            (vec![3, 4], vec![3, 4], vec![], false)
        );
        let repeated = [
            node(0, 1, Media::Alternative),
            node(1, 2, Media::Plain),
            node(1, 2, Media::Plain),
            node(1, 2, Media::Html),
            node(1, 2, Media::Html),
        ];
        assert_eq!(run(&repeated), (vec![2, 3], vec![4, 5], vec![], false));
        let nested = [
            node(0, 1, Media::Alternative),
            node(1, 2, Media::Multipart),
            node(2, 3, Media::Plain),
            node(2, 3, Media::Alternative),
            node(4, 4, Media::Plain),
            node(4, 4, Media::Html),
        ];
        // The nested alternative inherits a disabled HTML channel; outer fallback
        // copies completed plaintext, while the unlisted HTML leaf is an attachment.
        assert_eq!(run(&nested), (vec![3, 5], vec![3, 5], vec![6], true));
        assert_eq!(
            run(&[node(0, 1, Media::Plain)]),
            (vec![1], vec![1], vec![], false)
        );
        assert_eq!(
            run(&[node(0, 1, Media::Other)]),
            (vec![], vec![], vec![1], true)
        );
    }
    #[test]
    fn classification_charges_original_owners_and_empty_name_is_not_named() {
        use crate::{header_select::SourceEnd, metadata::Context, nfc::Scratch, part_headers};
        for empty in [false, true] {
            let source = if empty {
                b"Content-Disposition:inline;filename=\"\"\n\n".as_slice()
            } else {
                b"Content-Type:IMAGE/JPEG\nContent-Disposition:ATTACHMENT;filename=x\n\n"
            };
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut scratch = Scratch::new();
            let mut h = [0; 64];
            let mut c = [];
            let mut n = [0; 16];
            let mut headers = part_headers::Cursor::new(
                part_headers::Entity {
                    source,
                    base: 0,
                    source_end: SourceEnd::Eof,
                    header_limit: 1024,
                    context: Context::Normal,
                },
                part_headers::Backing {
                    heads: &mut h,
                    charset: &mut c,
                    filename: &mut n,
                },
                &mut work,
                &mut budget,
                &mut scratch,
            )
            .unwrap();
            let mut done = false;
            for _ in 0..1000 {
                if headers.poll(Tick(1)).unwrap() == part_headers::Status::Complete {
                    done = true;
                    break;
                }
            }
            assert!(done);
            let (view, work, budget, _) = headers.finish(Tick(1)).unwrap();
            let before = work.remaining();
            let bytes = budget.source_bytes_remaining();
            let steps = budget.steps_remaining();
            let class = Class::from_headers(view, Tick(1), work, budget).unwrap();
            assert_eq!(
                class.media,
                if empty {
                    Media::Plain
                } else {
                    Media::InlineMedia
                }
            );
            assert_eq!(
                class.disposition,
                if empty {
                    Disposition::Inline
                } else {
                    Disposition::Attachment
                }
            );
            assert_eq!(class.named, !empty);
            assert_eq!(bytes - budget.source_bytes_remaining(), 64);
            assert_eq!(steps - budget.steps_remaining(), 64);
            assert_eq!(before.io_bytes - work.remaining().io_bytes, 64);
            assert_eq!(before.records - work.remaining().records, 4);
            assert_eq!(
                before.output_bytes - work.remaining().output_bytes,
                std::mem::size_of::<Class>() as u64
            );
            assert_eq!(
                Class::from_headers(view, Tick(100), work, budget),
                Err(Error::Work(Stop::Deadline))
            );
        }
    }
    #[test]
    fn malformed_preorder_depth_parts_and_backing_fail_without_partial_lists() {
        let cases = [
            vec![node(0, 0, Media::Plain)],
            vec![node(0, 2, Media::Plain)],
            vec![node(1, 1, Media::Plain)],
            vec![node(0, 1, Media::Plain), node(1, 2, Media::Plain)],
            vec![node(0, 1, Media::Multipart), node(8, 2, Media::Plain)],
            vec![node(0, 1, Media::Multipart), node(1, 3, Media::Plain)],
            vec![node(0, 1, Media::Plain), node(0, 0, Media::Plain)],
            vec![node(0, 1, Media::Plain), node(0, 1, Media::Plain)],
        ];
        for nodes in &cases {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut t = [0; 8];
            let mut h = [0; 8];
            let mut a = [0; 8];
            let mut f = [0; 8];
            let mut cursor = Cursor::new(
                nodes,
                Limits::default(),
                Backing {
                    text: &mut t,
                    html: &mut h,
                    attachments: &mut a,
                    membership: &mut f,
                },
                &mut work,
                &mut budget,
            )
            .unwrap();
            assert_eq!(drain(&mut cursor), Err(Error::InvalidTree));
            assert!(cursor.value().is_none());
            assert_eq!(cursor.poll(Tick(1)), Err(Error::InvalidTree));
            assert!(matches!(cursor.finish(Tick(1)), Err(Error::InvalidTree)));
        }
        for flavor in 0..4 {
            let nodes = if flavor == 2 {
                [node(0, 1, Media::Other)]
            } else {
                [node(0, 1, Media::Plain)]
            };
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut t = [0; 1];
            let mut h = [0; 1];
            let mut a = [0; 1];
            let mut f = [0; 1];
            let mut cursor = Cursor::new(
                &nodes,
                Limits::default(),
                Backing {
                    text: if flavor == 0 { &mut [] } else { &mut t },
                    html: if flavor == 1 { &mut [] } else { &mut h },
                    attachments: if flavor == 2 { &mut [] } else { &mut a },
                    membership: if flavor == 3 { &mut [] } else { &mut f },
                },
                &mut work,
                &mut budget,
            )
            .unwrap();
            assert_eq!(drain(&mut cursor), Err(Error::OutputCapacity));
            assert!(cursor.value().is_none());
        }
    }
    #[test]
    fn all_job_cuts_and_deadline_turns_retire_fallback_and_sweep() {
        let nodes = [
            node(0, 1, Media::Alternative),
            node(1, 2, Media::Plain),
            node(1, 2, Media::InlineMedia),
        ];
        let mut work = meter();
        let before = work.remaining();
        let mut budget = HeaderBudget::new();
        let mut t = [0; 3];
        let mut h = [0; 3];
        let mut a = [0; 3];
        let mut f = [0; 3];
        let turns = {
            let mut cursor = Cursor::new(
                &nodes,
                Limits::default(),
                Backing {
                    text: &mut t,
                    html: &mut h,
                    attachments: &mut a,
                    membership: &mut f,
                },
                &mut work,
                &mut budget,
            )
            .unwrap();
            let mut count = 0;
            loop {
                count += 1;
                if cursor.poll(Tick(1)).unwrap() == Status::Complete {
                    break count;
                }
            }
        };
        let used = [
            before.io_bytes - work.remaining().io_bytes,
            before.records - work.remaining().records,
            before.output_bytes - work.remaining().output_bytes,
        ];
        for (flavor, total) in used.into_iter().enumerate() {
            for cap in 0..total {
                let mut allowance = meter().remaining();
                match flavor {
                    0 => allowance.io_bytes = cap,
                    1 => allowance.records = cap,
                    2 => allowance.output_bytes = cap,
                    _ => panic!("invalid cut"),
                };
                let mut work = Meter::new(Deadline::after(Tick(0), 100).unwrap(), allowance);
                let mut budget = HeaderBudget::new();
                let mut t = [0; 3];
                let mut h = [0; 3];
                let mut a = [0; 3];
                let mut f = [0; 3];
                let mut cursor = Cursor::new(
                    &nodes,
                    Limits::default(),
                    Backing {
                        text: &mut t,
                        html: &mut h,
                        attachments: &mut a,
                        membership: &mut f,
                    },
                    &mut work,
                    &mut budget,
                )
                .unwrap();
                let expected = Error::Work(match flavor {
                    0 => Stop::IoBytes,
                    1 => Stop::Records,
                    2 => Stop::OutputBytes,
                    _ => panic!("invalid cut"),
                });
                assert_eq!(drain(&mut cursor), Err(expected));
                assert!(cursor.value().is_none());
                let remaining = cursor.work.remaining();
                assert_eq!(cursor.poll(Tick(1)), Err(expected));
                assert_eq!(cursor.check_deadline(Tick(1)), Err(expected));
                assert_eq!(cursor.work.remaining(), remaining);
            }
        }
        for turn in 0..=turns {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut t = [0; 3];
            let mut h = [0; 3];
            let mut a = [0; 3];
            let mut f = [0; 3];
            let identities = (std::ptr::from_ref(&work), std::ptr::from_ref(&budget));
            let mut cursor = Cursor::new(
                &nodes,
                Limits::default(),
                Backing {
                    text: &mut t,
                    html: &mut h,
                    attachments: &mut a,
                    membership: &mut f,
                },
                &mut work,
                &mut budget,
            )
            .unwrap();
            for _ in 0..turn {
                cursor.poll(Tick(1)).unwrap();
            }
            if turn == turns {
                assert!(cursor.value().is_some());
                assert_eq!(cursor.poll(Tick(100)), Ok(Status::Complete));
                let (_, work, budget) = cursor.finish(Tick(1)).unwrap();
                assert_eq!(
                    (std::ptr::from_ref(&*work), std::ptr::from_ref(&*budget)),
                    identities
                );
            } else {
                assert_eq!(cursor.poll(Tick(100)), Err(Error::Work(Stop::Deadline)));
                assert!(cursor.value().is_none());
            }
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut t = [0; 3];
        let mut h = [0; 3];
        let mut a = [0; 3];
        let mut f = [0; 3];
        let mut cursor = Cursor::new(
            &nodes,
            Limits::default(),
            Backing {
                text: &mut t,
                html: &mut h,
                attachments: &mut a,
                membership: &mut f,
            },
            &mut work,
            &mut budget,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        let bytes = cursor.budget.source_bytes_remaining();
        cursor
            .budget
            .charge_local(cursor.work, Tick(1), bytes + 1, 0, &mut 0)
            .unwrap_err();
        assert_eq!(
            cursor.check_deadline(Tick(1)),
            Err(Error::InterpretationLimit)
        );
        assert!(cursor.value().is_none());
    }
    #[test]
    fn maximum_depth_and_part_count() {
        let mut deep = Vec::new();
        for depth in 1..=64 {
            deep.push(node(
                depth - 1,
                depth as u8,
                if depth == 64 {
                    Media::Plain
                } else {
                    Media::Alternative
                },
            ));
        }
        assert_eq!(run(&deep), (vec![64], vec![64], vec![], false));
        let mut wide = vec![node(0, 1, Media::Multipart)];
        wide.extend((1..4096).map(|_| node(1, 2, Media::Plain)));
        let (text, html, attachments, has) = run(&wide);
        assert_eq!(text, (2..=4096).collect::<Vec<_>>());
        assert_eq!(html, text);
        assert!(attachments.is_empty());
        assert!(!has);
    }
    #[test]
    fn constructor_limits_and_fresh_consuming_refusals() {
        let nodes = [node(0, 1, Media::Multipart), node(1, 2, Media::Plain)];
        for (depth, parts, expected) in [
            (0, 1024, Error::InvalidLimits),
            (65, 1024, Error::InvalidLimits),
            (1, 0, Error::InvalidLimits),
            (1, 4097, Error::InvalidLimits),
            (2, 1, Error::InvalidLimits),
            (1, 1, Error::PartLimit),
        ] {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut t = [0; 2];
            let mut h = [0; 2];
            let mut a = [0; 2];
            let mut f = [0; 2];
            let limits = Limits {
                mime_depth: depth,
                mime_parts: parts,
                ..Limits::default()
            };
            let result = Cursor::new(
                &nodes,
                limits,
                Backing {
                    text: &mut t,
                    html: &mut h,
                    attachments: &mut a,
                    membership: &mut f,
                },
                &mut work,
                &mut budget,
            );
            assert!(matches!(result, Err(error) if error == expected));
        }
        for mode in 0..3 {
            let mut work = meter();
            let mut budget = HeaderBudget::new();
            let mut t = [0; 2];
            let mut h = [0; 2];
            let mut a = [0; 2];
            let mut f = [0; 2];
            let limits = Limits {
                mime_depth: if mode == 0 { 1 } else { 32 },
                ..Limits::default()
            };
            let result = Cursor::new(
                if mode == 2 { &[] } else { &nodes },
                limits,
                Backing {
                    text: &mut t,
                    html: &mut h,
                    attachments: &mut a,
                    membership: &mut f,
                },
                &mut work,
                &mut budget,
            );
            if mode == 2 {
                assert!(matches!(result, Err(Error::InvalidTree)));
                continue;
            }
            let mut cursor = result.unwrap();
            if mode == 0 {
                assert_eq!(drain(&mut cursor), Err(Error::DepthLimit));
            } else if mode == 1 {
                drain(&mut cursor).unwrap();
                assert_eq!(
                    cursor.check_deadline(Tick(100)),
                    Err(Error::Work(Stop::Deadline))
                );
            }
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut t = [0; 2];
        let mut h = [0; 2];
        let mut a = [0; 2];
        let mut f = [0; 2];
        let cursor = Cursor::new(
            &nodes,
            Limits::default(),
            Backing {
                text: &mut t,
                html: &mut h,
                attachments: &mut a,
                membership: &mut f,
            },
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert!(matches!(cursor.finish(Tick(1)), Err(Error::InvalidState)));
        let mut cursor = Cursor::new(
            &nodes,
            Limits::default(),
            Backing {
                text: &mut t,
                html: &mut h,
                attachments: &mut a,
                membership: &mut f,
            },
            &mut work,
            &mut budget,
        )
        .unwrap();
        drain(&mut cursor).unwrap();
        assert!(matches!(
            cursor.finish(Tick(100)),
            Err(Error::Work(Stop::Deadline))
        ));
        let mut deep = Vec::new();
        for depth in 1..=65 {
            deep.push(node(depth - 1, depth as u8, Media::Multipart));
        }
        let mut work = meter();
        let mut budget = HeaderBudget::new();
        let mut t = [0; 65];
        let mut h = [0; 65];
        let mut a = [0; 65];
        let mut f = [0; 65];
        let limits = Limits {
            mime_depth: 64,
            ..Limits::default()
        };
        let mut cursor = Cursor::new(
            &deep,
            limits,
            Backing {
                text: &mut t,
                html: &mut h,
                attachments: &mut a,
                membership: &mut f,
            },
            &mut work,
            &mut budget,
        )
        .unwrap();
        assert_eq!(drain(&mut cursor), Err(Error::DepthLimit));
        deep.pop();
        assert_eq!(run(&deep), (vec![], vec![], vec![], false));
    }
}
