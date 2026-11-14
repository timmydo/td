//! Fallible prose wrapping. Every retained array and temporary owns admission.
use super::{pixels, shown, GlyphStyle, Items, Metrics, Range, Run};
use td_alloc::{Budget, BudgetError, BudgetVec};

/// The part of one run on one line: the run's index and tag, its style,
/// the bytes of its text the line shows, the pixels it spans from the
/// line's left (`left` inclusive, `right` exclusive) and its glyphs in the
/// line's list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Piece {
    pub run: usize,
    pub tag: usize,
    pub style: GlyphStyle,
    pub text: Range<usize>,
    pub left: u32,
    pub right: u32,
    pub glyphs: Range<usize>,
}

/// A glyph on a line, its pen `x` pixels from the line's left.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Placed {
    pub x: u32,
    pub scalar: char,
}

/// One wrapped line: its width in pixels, which its trailing spaces do
/// not count, its pieces in order and its glyphs.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct Line {
    pub width: u32,
    pub pieces: Items<Piece>,
    pub glyphs: Items<Placed>,
}

impl Line {
    pub fn try_clone(&self) -> Result<Self, BudgetError> {
        Ok(Self {
            width: self.width,
            pieces: self.pieces.try_clone_with(|p| {
                Ok(Piece {
                    text: p.text.start..p.text.end,
                    glyphs: p.glyphs.start..p.glyphs.end,
                    ..*p
                })
            })?,
            glyphs: self.glyphs.try_clone_with(|g| Ok(*g))?,
        })
    }

    /// The piece under a point `x` pixels from the line's left; none past
    /// its ends or between pieces.
    pub fn hit(&self, x: i64) -> Option<&Piece> {
        self.pieces
            .iter()
            .find(|piece| i64::from(piece.left) <= x && x < i64::from(piece.right))
    }
}

/// A glyph while its line is laid: its run, where its scalar is in the
/// run's text, and its pen and advance in `UNIT`s from the line's left.
#[derive(Clone, Copy)]
struct Pending {
    run: usize,
    tag: usize,
    style: GlyphStyle,
    at: usize,
    len: usize,
    scalar: char,
    pen: u64,
    advance: u64,
    space: bool,
}

/// Lays `runs` into lines no wider than `width` pixels, measured with
/// `metrics`: a line breaks after a space, and inside a word only when the
/// word alone is wider than a line; spaces at a line's end take no width
/// and are dropped with the break, and a line never ends holding only
/// spaces: those before a word that moves to the next line go with the
/// break, leading spaces included; a newline ends a line and is not shown,
/// and a line holds at least one glyph however narrow. Each glyph's pen is
/// its advances summed in `UNIT`s and then rounded, so a line's width does
/// not drift with its length. There is always one line, empty for empty
/// text, and one more after each newline. Work and memory are linear in
/// the runs' text: a consumer bounds what it hands over.
pub fn wrap(
    metrics: &mut Metrics,
    runs: &[Run],
    width: u32,
    budget: &Budget,
) -> Result<BudgetVec<Line>, BudgetError> {
    wrap_at_most(metrics, runs, width, usize::MAX, budget)
}

/// `wrap`'s first `most` lines, none for none: it stops laying out once it
/// has them, so a consumer that keeps a bounded number of lines pays for
/// no more however narrow the width or long the text.
pub fn wrap_at_most(
    metrics: &mut Metrics,
    runs: &[Run],
    width: u32,
    most: usize,
    budget: &Budget,
) -> Result<BudgetVec<Line>, BudgetError> {
    let limit = u64::from(width);
    let mut lines = td_alloc::BudgetVec::with_capacity(budget, most.min(4))?;
    if most == 0 {
        return Ok(lines);
    }
    let mut initial = 0usize;
    for run in runs {
        initial = initial.saturating_add(run.text.len()).min(64);
        if initial == 64 {
            break;
        }
    }
    let mut line: BudgetVec<Pending> = td_alloc::BudgetVec::with_capacity(budget, initial)?;
    let mut pen = 0u64;
    // Where the line may break: the glyphs before this index stay.
    let mut after_space: Option<usize> = None;
    for (index, run) in runs.iter().enumerate() {
        for (at, scalar) in run.text.char_indices() {
            if scalar == '\n' {
                lines.try_push(finish(&line, budget)?)?;
                if lines.len() >= most {
                    return Ok(lines);
                }
                line.clear();
                (pen, after_space) = (0, None);
                continue;
            }
            let shown = shown(scalar);
            let space = shown == ' ';
            let advance = u64::from(metrics.advance(run.style.family, run.style.font_style, shown));
            let over = |pen: u64| u64::from(pixels(pen.saturating_add(advance))) > limit;
            if !space && over(pen) && !line.is_empty() {
                // The word so far moves to a new line; if it is alone
                // wider than a line, it breaks here instead.
                let keep = after_space.take().unwrap_or(line.len()).min(line.len());
                let prefix = line.get(..keep).unwrap_or(&[]);
                // A line never ends holding only spaces: those before a
                // word moved from them go with the break.
                if prefix.iter().any(|glyph| !glyph.space) {
                    lines.try_push(finish(prefix, budget)?)?;
                    if lines.len() >= most {
                        return Ok(lines);
                    }
                }
                let shift = line.get(keep).map_or(pen, |first| first.pen);
                let rest = line.len() - keep;
                line.copy_within(keep.., 0);
                line.truncate(rest);
                for glyph in &mut line {
                    glyph.pen = glyph.pen.saturating_sub(shift);
                }
                pen -= shift.min(pen);
                if over(pen) && !line.is_empty() {
                    lines.try_push(finish(&line, budget)?)?;
                    if lines.len() >= most {
                        return Ok(lines);
                    }
                    line.clear();
                    pen = 0;
                }
            }
            line.try_push(Pending {
                run: index,
                tag: run.tag,
                style: run.style,
                at,
                len: scalar.len_utf8(),
                scalar: shown,
                pen,
                advance,
                space,
            })?;
            pen = pen.saturating_add(advance);
            if space {
                after_space = Some(line.len());
            }
        }
    }
    lines.try_push(finish(&line, budget)?)?;
    Ok(lines)
}

/// A laid line from its pending glyphs, its trailing spaces dropped.
fn finish(pending: &[Pending], budget: &Budget) -> Result<Line, BudgetError> {
    let end = pending
        .iter()
        .rposition(|glyph| !glyph.space)
        .map_or(0, |last| last + 1);
    let pieces = pending
        .iter()
        .take(end)
        .enumerate()
        .filter(|(at, glyph)| {
            *at == 0
                || pending
                    .get(at - 1)
                    .is_none_or(|previous| previous.run != glyph.run)
        })
        .count();
    let mut line = Line {
        width: 0,
        pieces: Items::try_with_capacity(budget, pieces)?,
        glyphs: Items::try_with_capacity(budget, end)?,
    };
    for glyph in pending.iter().take(end) {
        let x = pixels(glyph.pen);
        let right = pixels(glyph.pen.saturating_add(glyph.advance));
        let next = line.glyphs.len();
        match line.pieces.last_mut() {
            Some(piece) if piece.run == glyph.run => {
                piece.text.end = glyph.at + glyph.len;
                piece.right = piece.right.max(right);
                piece.glyphs.end = next + 1;
            }
            _ => line.pieces.try_push(Piece {
                run: glyph.run,
                tag: glyph.tag,
                style: glyph.style,
                text: glyph.at..glyph.at + glyph.len,
                left: x,
                right,
                glyphs: next..next + 1,
            })?,
        }
        line.glyphs.try_push(Placed {
            x,
            scalar: glyph.scalar,
        })?;
        line.width = line.width.max(right);
    }
    Ok(line)
}
