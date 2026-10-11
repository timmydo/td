//! Blocks: CommonMark's container and leaf structure, line by line.

use std::ops::Range;

use crate::{inline, Block, BlockKind, Document, Level, Marker, Span, SpanKind, Style, MAX_DEPTH};

/// The columns a tab advances to the next multiple of.
const TAB: usize = 4;
/// The most digits an ordered list marker has.
const MAX_ORDER_DIGITS: usize = 9;

enum Container {
    Quote,
    Item {
        marker: Marker,
        /// The column its content starts at.
        content: usize,
        /// Whether a block of it has been emitted, which showed its marker.
        emitted: bool,
        /// Its marker's bytes, an empty item's source.
        at: Range<usize>,
        /// Whether it opened on a blank line: it closes at the next blank
        /// line while it holds nothing.
        opened_blank: bool,
    },
}

enum Leaf {
    None,
    Paragraph(Vec<Range<usize>>),
    Fenced {
        fence: u8,
        length: usize,
        indent: usize,
        info: Range<usize>,
        start: usize,
        /// Its last line's end, its closing fence's once it closes.
        end: usize,
        lines: Vec<Range<usize>>,
    },
    /// Its lines, trailing blank ones trimmed as it closes.
    Indented(Vec<Range<usize>>),
    Table(Vec<Range<usize>>),
}

/// A place in a line: its byte offset in the source and its column.
#[derive(Clone, Copy)]
struct At {
    pos: usize,
    col: usize,
}

pub(crate) struct Parser<'a> {
    source: &'a str,
    doc: Document,
    stack: Vec<Container>,
    leaf: Leaf,
    /// Whether a blank line came since the last block.
    blank: bool,
}

fn byte(source: &str, pos: usize) -> Option<u8> {
    source.as_bytes().get(pos).copied()
}

/// The whitespace at `at`: its columns, and where it ends.
fn indent(source: &str, end: usize, at: At) -> (usize, At) {
    let mut here = at;
    while here.pos < end {
        match byte(source, here.pos) {
            Some(b' ') => here.col += 1,
            Some(b'\t') => here.col += TAB - here.col % TAB,
            _ => break,
        }
        here.pos += 1;
    }
    (here.col - at.col, here)
}

/// Past up to `columns` columns of whitespace at `at`; a tab reaching past
/// them is taken whole.
fn skip(source: &str, end: usize, at: At, columns: usize) -> At {
    let mut here = at;
    let goal = at.col + columns;
    while here.pos < end && here.col < goal {
        match byte(source, here.pos) {
            Some(b' ') => here.col += 1,
            Some(b'\t') => here.col += TAB - here.col % TAB,
            _ => break,
        }
        here.pos += 1;
    }
    here
}

fn blank(text: &str) -> bool {
    text.bytes().all(|b| b == b' ' || b == b'\t')
}

/// A thematic break: three or more of one of `*`, `-` and `_`, with only
/// spaces and tabs between.
fn rule(text: &str) -> bool {
    let mut kind = None;
    let mut count = 0;
    for b in text.bytes() {
        match b {
            b' ' | b'\t' => {}
            b'*' | b'-' | b'_' if kind.is_none_or(|k| k == b) => {
                kind = Some(b);
                count += 1;
            }
            _ => return false,
        }
    }
    count >= 3
}

/// A setext underline: a run of `=` (level 1) or `-` (level 2), trailing
/// spaces allowed.
fn underline(text: &str) -> Option<u8> {
    let trimmed = text.trim_end_matches([' ', '\t']);
    let first = trimmed.bytes().next()?;
    let level = match first {
        b'=' => 1,
        b'-' => 2,
        _ => return None,
    };
    trimmed.bytes().all(|b| b == first).then_some(level)
}

/// An ATX heading at `start..end`: its level and its content's range.
fn atx(source: &str, start: usize, end: usize) -> Option<(u8, Range<usize>)> {
    let text = source.get(start..end)?;
    let hashes = text.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = text.get(hashes..)?;
    if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    let mut content = rest.trim_matches([' ', '\t']);
    // A closing run of `#`, the whole content or after a space.
    let closing = content.trim_end_matches('#');
    if closing.is_empty() || closing.ends_with([' ', '\t']) {
        content = closing.trim_end_matches([' ', '\t']);
    }
    let from = start + hashes + (rest.len() - rest.trim_start_matches([' ', '\t']).len());
    Some((hashes as u8, from..from + content.len()))
}

/// A fence opening at `start..end`: its character, length and info range.
fn fence(source: &str, start: usize, end: usize) -> Option<(u8, usize, Range<usize>)> {
    let text = source.get(start..end)?;
    let first = text.bytes().next()?;
    if first != b'`' && first != b'~' {
        return None;
    }
    let length = text.bytes().take_while(|&b| b == first).count();
    if length < 3 {
        return None;
    }
    let rest = text.get(length..)?;
    if first == b'`' && rest.contains('`') {
        return None;
    }
    let lead = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    let info = rest.trim_matches([' ', '\t']);
    let from = start + length + lead;
    Some((first, length, from..from + info.len()))
}

/// A list marker at `at`: the marker, and where it ends.
fn marker(source: &str, end: usize, at: At) -> Option<(Marker, At)> {
    let first = byte(source, at.pos)?;
    let (marker, width) = match first {
        b'-' | b'+' | b'*' => (Marker::Bullet(char::from(first)), 1),
        b'0'..=b'9' => {
            let digits = source
                .get(at.pos..end)?
                .bytes()
                .take_while(u8::is_ascii_digit)
                .count();
            if digits > MAX_ORDER_DIGITS {
                return None;
            }
            let delimiter = byte(source, at.pos + digits)?;
            if delimiter != b'.' && delimiter != b')' {
                return None;
            }
            let number = source.get(at.pos..at.pos + digits)?.parse().ok()?;
            (
                Marker::Ordered {
                    number,
                    delimiter: char::from(delimiter),
                },
                digits + 1,
            )
        }
        _ => return None,
    };
    let after = at.pos + width;
    match byte(source, after) {
        None | Some(b' ' | b'\t') => {}
        Some(_) if after >= end => {}
        Some(_) => return None,
    }
    Some((
        marker,
        At {
            pos: after,
            col: at.col + width,
        },
    ))
}

/// A pipe table's delimiter row: its cell count.
fn delimiter_row(text: &str) -> Option<usize> {
    let cells = cells(text);
    let mut count = 0;
    for cell in cells.split('|') {
        let cell = cell.trim_matches([' ', '\t']);
        let inner = cell.strip_prefix(':').unwrap_or(cell);
        let inner = inner.strip_suffix(':').unwrap_or(inner);
        if inner.is_empty() || !inner.bytes().all(|b| b == b'-') {
            return None;
        }
        count += 1;
    }
    Some(count)
}

/// A table row's cells, its outer pipes taken off; an escaped closing
/// pipe is a cell's.
fn cells(text: &str) -> &str {
    let text = text.trim_matches([' ', '\t']);
    let text = text.strip_prefix('|').unwrap_or(text);
    // An odd run of backslashes before the last pipe escapes it.
    let backslashes = text.strip_suffix('|').map_or(0, |head| {
        head.bytes().rev().take_while(|&b| b == b'\\').count()
    });
    if backslashes % 2 == 1 {
        return text;
    }
    text.strip_suffix('|').unwrap_or(text)
}

/// A row's cell count: its pipes between cells, an escaped one not one.
fn row_cells(text: &str) -> usize {
    let mut count = 1;
    let mut escaped = false;
    for b in cells(text).bytes() {
        match b {
            _ if escaped => escaped = false,
            b'\\' => escaped = true,
            b'|' => count += 1,
            _ => {}
        }
    }
    count
}

impl<'a> Parser<'a> {
    pub(crate) fn new(source: &'a str) -> Self {
        Self {
            source,
            doc: Document::default(),
            stack: Vec::new(),
            leaf: Leaf::None,
            blank: false,
        }
    }

    pub(crate) fn run(mut self) -> Document {
        let mut start = 0;
        while start <= self.source.len() {
            let end = self
                .source
                .get(start..)
                .and_then(|rest| rest.find('\n'))
                .map_or(self.source.len(), |at| start + at);
            // A final newline ends the last line; it begins none.
            if start == self.source.len() && start > 0 {
                break;
            }
            let content_end = if end > start && byte(self.source, end - 1) == Some(b'\r') {
                end - 1
            } else {
                end
            };
            self.line(start, content_end);
            start = end + 1;
        }
        self.close_to(0);
        self.doc
    }

    fn text(&self, range: Range<usize>) -> &'a str {
        self.source.get(range).unwrap_or("")
    }

    fn line(&mut self, start: usize, end: usize) {
        let source = self.source;
        let mut at = At { pos: start, col: 0 };
        let mut matched = 0;
        for container in &self.stack {
            match container {
                Container::Quote => {
                    let (columns, past) = indent(source, end, at);
                    if columns > 3 || byte(source, past.pos) != Some(b'>') || past.pos >= end {
                        break;
                    }
                    at = At {
                        pos: past.pos + 1,
                        col: past.col + 1,
                    };
                    at = skip(source, end, at, 1);
                }
                Container::Item { content, .. } => {
                    if blank(self.text(at.pos..end)) {
                        at.pos = end;
                    } else {
                        let (columns, _) = indent(source, end, at);
                        if at.col + columns < *content {
                            break;
                        }
                        // A tab an outer container took whole may already
                        // reach past the content column.
                        at = skip(source, end, at, content.saturating_sub(at.col));
                    }
                }
            }
            matched += 1;
        }
        let all = matched == self.stack.len();

        // An open fence takes every line of its containers until it closes.
        if let Leaf::Fenced {
            fence: kind,
            length,
            indent: fence_indent,
            ..
        } = &self.leaf
        {
            if all {
                let (columns, past) = indent(source, end, at);
                let rest = self.text(past.pos..end);
                let closes = columns <= 3
                    && rest.bytes().take_while(|b| b == kind).count() >= *length
                    && blank(rest.trim_start_matches(char::from(*kind)));
                let from = skip(source, end, at, *fence_indent);
                if let Leaf::Fenced {
                    lines, end: last, ..
                } = &mut self.leaf
                {
                    *last = end;
                    if !closes {
                        lines.push(from.pos..end);
                    }
                }
                if closes {
                    self.close_leaf();
                }
                return;
            }
            self.close_leaf();
        }

        let paragraph = matches!(self.leaf, Leaf::Paragraph(_));
        let mut opened = false;
        while self.stack.len() < MAX_DEPTH {
            let (columns, past) = indent(source, end, at);
            if columns > 3 {
                break;
            }
            if byte(source, past.pos) == Some(b'>') && past.pos < end {
                self.close_to(matched);
                self.stack.push(Container::Quote);
                matched += 1;
                at = skip(
                    source,
                    end,
                    At {
                        pos: past.pos + 1,
                        col: past.col + 1,
                    },
                    1,
                );
                opened = true;
                continue;
            }
            if rule(self.text(past.pos..end)) {
                break;
            }
            let Some((found, after)) = marker(source, end, past) else {
                break;
            };
            let rest = self.text(after.pos..end);
            // An item interrupting a paragraph has text, and an ordered one
            // starts at 1.
            // Only a paragraph its containers all matched is interrupted;
            // under one they did not, an item starts a list of its own.
            let interrupts = paragraph && all && !opened;
            if interrupts
                && (blank(rest) || matches!(found, Marker::Ordered { number, .. } if number != 1))
            {
                break;
            }
            let (spaces, past_spaces) = indent(source, end, after);
            let (content, next) = if blank(rest) || spaces >= 5 {
                (after.col + 1, skip(source, end, after, 1))
            } else {
                (after.col + spaces, past_spaces)
            };
            self.close_to(matched);
            self.stack.push(Container::Item {
                marker: found,
                content,
                emitted: false,
                at: past.pos..after.pos,
                opened_blank: blank(rest),
            });
            matched += 1;
            at = next;
            opened = true;
        }

        let rest = self.text(at.pos..end);
        if blank(rest) {
            if !all && !opened {
                self.close_to(matched);
            }
            // An item that opened blank and still holds nothing ends here,
            // before the blank line, which stands after it.
            let empty = matches!(
                self.stack.last(),
                Some(Container::Item {
                    emitted: false,
                    opened_blank: true,
                    ..
                })
            ) && matches!(self.leaf, Leaf::None);
            if empty && !opened {
                self.close_to(self.stack.len().saturating_sub(1));
            }
            // An indented block holds its blank lines until it closes; a
            // line that opens an empty item is no blank line.
            match &mut self.leaf {
                Leaf::Indented(lines) => lines.push(end..end),
                Leaf::Paragraph(_) | Leaf::Table(_) => {
                    self.close_leaf();
                    self.blank = !opened;
                }
                Leaf::None | Leaf::Fenced { .. } => self.blank |= !opened,
            }
            return;
        }

        let (columns, past) = indent(source, end, at);
        let lead = self.text(past.pos..end);
        let starts_block = columns <= 3
            && (rule(lead)
                || atx(source, past.pos, end).is_some()
                || fence(source, past.pos, end).is_some());
        // A lazy continuation: a paragraph's line its containers did not
        // all match.
        if !all && !opened && paragraph && !starts_block {
            if let Leaf::Paragraph(lines) = &mut self.leaf {
                lines.push(past.pos..end);
            }
            return;
        }
        if !all {
            self.close_to(matched);
        }

        if columns >= 4 && !matches!(self.leaf, Leaf::Paragraph(_)) {
            let from = skip(source, end, at, 4);
            if !matches!(self.leaf, Leaf::Indented(_)) {
                self.close_leaf();
                self.leaf = Leaf::Indented(Vec::new());
            }
            if let Leaf::Indented(lines) = &mut self.leaf {
                lines.push(from.pos..end);
            }
            return;
        }
        if columns <= 3 {
            if let (Leaf::Paragraph(lines), Some(level)) = (&self.leaf, underline(lead)) {
                let lines = lines.clone();
                self.leaf = Leaf::None;
                let source_start = lines.first().map_or(past.pos, |line| line.start);
                self.inline_block(BlockKind::Heading(level), &lines, source_start..end);
                return;
            }
            if rule(lead) {
                self.close_leaf();
                self.emit(BlockKind::Rule, Vec::new(), past.pos..end);
                return;
            }
            if let Some((level, content)) = atx(source, past.pos, end) {
                self.close_leaf();
                self.inline_block(BlockKind::Heading(level), &[content], past.pos..end);
                return;
            }
            if let Some((kind, length, info)) = fence(source, past.pos, end) {
                self.close_leaf();
                self.leaf = Leaf::Fenced {
                    fence: kind,
                    length,
                    indent: columns,
                    end: info.end,
                    info,
                    start: past.pos,
                    lines: Vec::new(),
                };
                return;
            }
        }
        // A pipe table: a paragraph's last line, with a pipe, over a
        // delimiter row of as many cells; the lines before it stay a
        // paragraph. Every line after is a row until a blank line or another
        // block.
        if let Leaf::Paragraph(lines) = &mut self.leaf {
            let header = lines.last().cloned().unwrap_or(0..0);
            let header_text = self.source.get(header.clone()).unwrap_or("");
            if header_text.contains('|') && delimiter_row(lead) == Some(row_cells(header_text)) {
                lines.pop();
                self.close_leaf();
                self.leaf = Leaf::Table(vec![header, past.pos..end]);
                return;
            }
        }
        if let Leaf::Table(lines) = &mut self.leaf {
            lines.push(past.pos..end);
            return;
        }
        match &mut self.leaf {
            Leaf::Paragraph(lines) => lines.push(past.pos..end),
            _ => {
                self.close_leaf();
                self.leaf = Leaf::Paragraph(std::iter::once(past.pos..end).collect());
            }
        }
    }

    /// Closes the open leaf, then the containers past the first `depth`; an
    /// item closed with no block of its own shows its marker on an empty
    /// paragraph.
    fn close_to(&mut self, depth: usize) {
        self.close_leaf();
        while self.stack.len() > depth {
            if let Some(Container::Item {
                emitted: false, at, ..
            }) = self.stack.last()
            {
                let at = at.clone();
                self.emit(BlockKind::Paragraph, Vec::new(), at);
            }
            self.stack.pop();
        }
    }

    fn close_leaf(&mut self) {
        match std::mem::replace(&mut self.leaf, Leaf::None) {
            Leaf::None => {}
            Leaf::Paragraph(lines) => {
                let source = match (lines.first(), lines.last()) {
                    (Some(first), Some(last)) => first.start..last.end,
                    _ => return,
                };
                self.inline_block(BlockKind::Paragraph, &lines, source);
            }
            Leaf::Fenced {
                info,
                start,
                end,
                lines,
                ..
            } => {
                let spans = self.verbatim(&lines);
                self.emit(BlockKind::Code { info }, spans, start..end);
            }
            Leaf::Indented(mut lines) => {
                let mut trailing = false;
                while lines
                    .last()
                    .is_some_and(|line| blank(self.text(line.clone())))
                {
                    lines.pop();
                    trailing = true;
                }
                let source = match (lines.first(), lines.last()) {
                    (Some(first), Some(last)) => first.start..last.end,
                    _ => return,
                };
                let spans = self.verbatim(&lines);
                self.emit(BlockKind::Code { info: 0..0 }, spans, source);
                // Its trailing blank lines stand before what follows.
                self.blank = trailing;
            }
            Leaf::Table(lines) => {
                let source = match (lines.first(), lines.last()) {
                    (Some(first), Some(last)) => first.start..last.end,
                    _ => return,
                };
                let spans = self.verbatim(&lines);
                self.emit(BlockKind::Table, spans, source);
            }
        }
    }

    /// A code block's or table's lines as written, a hard break at each
    /// line's end but the last's.
    fn verbatim(&self, lines: &[Range<usize>]) -> Vec<Span> {
        let mut spans = Vec::with_capacity(lines.len() * 2);
        let mut previous: Option<usize> = None;
        for line in lines {
            if let Some(end) = previous {
                spans.push(Span {
                    kind: SpanKind::HardBreak,
                    range: end..end,
                    style: Style::PLAIN,
                    link: None,
                });
            }
            previous = Some(line.end);
            if !line.is_empty() {
                spans.push(Span {
                    kind: SpanKind::Text,
                    range: line.clone(),
                    style: Style::PLAIN,
                    link: None,
                });
            }
        }
        spans
    }

    fn inline_block(&mut self, kind: BlockKind, lines: &[Range<usize>], source: Range<usize>) {
        let spans = inline::parse(self.source, lines, &mut self.doc.links);
        self.emit(kind, spans, source);
    }

    fn emit(&mut self, kind: BlockKind, spans: Vec<Span>, source: Range<usize>) {
        let levels_start = self.doc.levels.len();
        for container in &mut self.stack {
            self.doc.levels.push(match container {
                Container::Quote => Level::Quote,
                Container::Item {
                    marker, emitted, ..
                } => {
                    let first = !*emitted;
                    *emitted = true;
                    Level::Item {
                        marker: *marker,
                        first,
                    }
                }
            });
        }
        let spans_start = self.doc.spans.len();
        self.doc.spans.extend(spans);
        self.doc.blocks.push(Block {
            kind,
            levels: levels_start..self.doc.levels.len(),
            spans: spans_start..self.doc.spans.len(),
            source,
            blank_before: std::mem::take(&mut self.blank),
        });
    }
}
