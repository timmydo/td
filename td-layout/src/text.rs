//! A text page's lines counted at a width through the layout's own line
//! breaking, without building a layout (DESIGN.md §6, "Text pages").
use crate::block::{content_width, MAX_VIEWPORT};
use crate::inline::{
    white_space, Counters, Flow, FontMetrics, Line, LineBounds, LineSink, TextRun,
};
use crate::list::MarkerFragment;
use crate::{BoxId, BoxKind, Error, StyledDocument};
use std::sync::atomic::AtomicBool;
use td_alloc::BudgetVec;

/// Bytes of a source line laid at a time: a count past the cap stops
/// within this much of where it passed.
const CHUNK: usize = 16 * 1024;

/// What a text page lays at one width, or the part of it that fits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fit {
    /// Lines `block::layout` lays the kept text in at this width.
    pub laid: usize,
    /// `laid` and each indent dropped before a word that moved: no wider
    /// viewport lays the kept text in more (DESIGN.md §6). A cut within
    /// a line where a word split after its dropped indent counts that
    /// indent though the kept text keeps it: one more than the kept
    /// text's own count, never fewer.
    pub lines: usize,
    /// Where the text is cut, a byte offset in its text node, or None
    /// when it fits whole. Within the source line that passes the cap,
    /// where its first laid line past the cap would start, when that
    /// keeps at least one of its laid lines; else at the end of the last
    /// source line kept, before its newline, or 0 when none is.
    pub end: Option<usize>,
    /// Work units charged, as `Layout::work` counts them.
    pub work: usize,
}

/// Counts the lines of a text page in a viewport `viewport` CSS px wide,
/// as `block::layout` lays it, through its own breaking and with its text
/// block's width found as layout finds it, and cuts it where
/// `Fit::lines` would pass `cap` (at least 1).
///
/// A text page is one text node, laid `pre-wrap` or `break-spaces` in an
/// ordinary block, every other box an ordinary block: no inline box,
/// image, `<br>`, table or list marker. Its text holds no control
/// scalar but tab, newline and carriage return, so a laid line holds one
/// fragment at most. Anything else is Invalid; a document with no text
/// lays none. Charged and cancelled as a layout is, nothing retained.
/// Lines past `Limits::lines` refuse `MAX_LINES` as layout's do: the
/// count runs up to `CHUNK` bytes past the cap, so a cap that near them
/// can be refused rather than cut.
pub fn fit(
    styled: &StyledDocument<'_>,
    metrics: &mut impl FontMetrics,
    viewport: f64,
    cap: usize,
    cancel: Option<&AtomicBool>,
) -> Result<Fit, Error> {
    if !viewport.is_finite() || !(0.0..=MAX_VIEWPORT).contains(&viewport) {
        return Err(Error::Limit("MAX_VIEWPORT"));
    }
    if cap == 0 {
        return Err(Error::Invalid);
    }
    let mut counters = Counters::new(styled, cancel);
    counters.step(0)?;
    let Some((run, block)) = page_text(styled)? else {
        return Ok(Fit {
            laid: 0,
            lines: 0,
            end: None,
            work: counters.work,
        });
    };
    let width = content_width(styled, block, viewport, 0)?;
    let strut = styled.box_node(block).ok_or(Error::Invalid)?.style;
    let mut pending = BudgetVec::new(styled.budget())?;
    let flow = Flow::new(
        styled,
        metrics,
        width.floor() as u32,
        strut,
        0.0,
        styled.budget(),
        &mut pending,
    )?;
    let (laid, lines, end) = count(styled, flow, &run, cap, &mut counters)?;
    Ok(Fit {
        laid,
        lines,
        end,
        work: counters.work,
    })
}
/// The text node's run and the block it is laid in, if the document is a
/// text page.
fn page_text(styled: &StyledDocument<'_>) -> Result<Option<(TextRun, BoxId)>, Error> {
    if styled.has_tables()
        || styled.has_nested_lines()
        || styled.has_flex()
        || styled.has_grid()
        || styled.marker_count() != 0
    {
        return Err(Error::Invalid);
    }
    let mut found = None;
    for b in styled.boxes() {
        match b.kind {
            BoxKind::Block => {}
            BoxKind::Text if found.is_none() => {
                let block = b.parent.ok_or(Error::Invalid)?;
                found = Some((b.node, b.style, block));
            }
            _ => return Err(Error::Invalid),
        }
    }
    let Some((node, style, block)) = found else {
        return Ok(None);
    };
    let laid = styled.style(style).ok_or(Error::Invalid)?;
    if white_space(laid) != (true, true, true) {
        return Err(Error::Invalid);
    }
    let text = styled
        .document()
        .node(node)
        .and_then(|n| n.text())
        .ok_or(Error::Invalid)?;
    // Shown as U+FFFD, each its own fragment.
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
    {
        return Err(Error::Invalid);
    }
    Ok(Some((
        TextRun {
            node,
            range: 0..text.len(),
            style,
        },
        block,
    )))
}
/// The first laid line past the cap: where it starts, if it has an item,
/// and the lines laid and indents dropped before it.
#[derive(Clone, Copy)]
struct Passed {
    start: Option<usize>,
    laid: usize,
    indents: usize,
}
/// Lines counted, not kept.
struct Count {
    cap: usize,
    passed: Option<Passed>,
}
impl LineSink for Count {
    fn measure(&mut self, bounds: LineBounds, counters: &mut Counters) -> Result<f64, Error> {
        if self.passed.is_none() && counters.lines.saturating_add(counters.indents) > self.cap {
            self.passed = Some(Passed {
                start: bounds.start,
                laid: counters.lines.saturating_sub(1),
                indents: counters.indents,
            });
        }
        Ok(0.0)
    }
    fn wants(&self, _: f64, _: f64) -> bool {
        false
    }
    fn line(
        &mut self,
        _: Line,
        _: Option<MarkerFragment>,
        _: Option<(f64, f64)>,
        _: &mut Counters,
    ) -> Result<(), Error> {
        Ok(())
    }
}
/// Lays `run` source line by source line, as one inline sequence: the
/// lines laid, with dropped indents, and the cut.
fn count<M: FontMetrics>(
    styled: &StyledDocument<'_>,
    mut flow: Flow<'_, '_, '_, M>,
    run: &TextRun,
    cap: usize,
    counters: &mut Counters,
) -> Result<(usize, usize, Option<usize>), Error> {
    let text = styled
        .document()
        .node(run.node)
        .and_then(|n| n.text())
        .ok_or(Error::Invalid)?;
    let bytes = text.as_bytes();
    let mut sink = Count { cap, passed: None };
    // The last source line kept whole: its end before its newline, and
    // what text cut there lays.
    let mut kept = (0, 0, 0);
    // The lines laid before the source line being laid.
    let mut before = 0;
    let mut at = 0;
    while at < text.len() {
        let rest = bytes.get(at..).ok_or(Error::Invalid)?;
        let (content, terminated) = match rest.iter().position(|b| matches!(b, b'\n' | b'\r')) {
            Some(n) => (at + n, true),
            None => (text.len(), false),
        };
        let mut next = content;
        if terminated {
            next += 1;
            if bytes.get(content) == Some(&b'\r') && bytes.get(next) == Some(&b'\n') {
                next += 1;
            }
        }
        let mut from = at;
        while from < next && sink.passed.is_none() {
            let mut to = from.saturating_add(CHUNK).min(next);
            while !text.is_char_boundary(to) {
                to += 1;
            }
            flow.text(
                &TextRun {
                    node: run.node,
                    range: from..to,
                    style: run.style,
                },
                counters,
                &mut sink,
            )?;
            from = to;
        }
        if let Some(passed) = sink.passed {
            return Ok(cut(passed, before, kept));
        }
        // What this line's newline emitted is what text cut before the
        // newline lays at its end, but before a first line's newline,
        // which is no text.
        if terminated && content > 0 {
            let lines = counters.lines.saturating_add(counters.indents);
            kept = (content, counters.lines, lines);
        }
        at = next;
        // The next source line's, or the empty line a text ending in a
        // newline lays; an unterminated last line's last laid line is
        // still to come, from finish.
        if terminated {
            before = counters.lines;
        }
    }
    flow.finish(counters, &mut sink)?;
    Ok(match sink.passed {
        Some(passed) => cut(passed, before, kept),
        None => (
            counters.lines,
            counters.lines.saturating_add(counters.indents),
            None,
        ),
    })
}
/// Within the passing source line, `before` lines laid ahead of it, when
/// that keeps one of its lines: text cut where a soft break starts a
/// line lays at most the lines before it, its indent dropped as it was,
/// and one fewer with that indent counted where the break split a word
/// that moved past it, which then keeps it. Else at the last line kept
/// whole.
fn cut(
    passed: Passed,
    before: usize,
    kept: (usize, usize, usize),
) -> (usize, usize, Option<usize>) {
    match passed.start {
        Some(start) if passed.laid > before => (
            passed.laid,
            passed.laid.saturating_add(passed.indents),
            Some(start),
        ),
        _ => (kept.1, kept.2, Some(kept.0)),
    }
}

#[cfg(test)]
mod tests;
