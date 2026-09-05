//! Paragraph reflow. Only two endpoints are mapped; there is no per-scalar
//! offset table or word vector proportional to the document size.

use crate::text::{self, MAX_FILE_BYTES};
use crate::{Error, Result};
use std::ops::Range;

#[derive(Debug, Eq, PartialEq)]
pub struct Edit {
    pub range: Range<usize>,
    pub insert: String,
    pub anchor: usize,
    pub caret: usize,
}

fn indent(line: &str) -> &str {
    let len = line
        .bytes()
        .take_while(|b| matches!(b, b' ' | b'\t'))
        .count();
    line.get(..len).unwrap_or("")
}

fn get(text: &str, range: Range<usize>) -> Result<&str> {
    text.get(range).ok_or(Error::InvalidPosition)
}

fn append(out: &mut String, value: &str) -> Result<()> {
    if out
        .len()
        .checked_add(value.len())
        .is_none_or(|n| n > MAX_FILE_BYTES)
    {
        return Err(Error::Limit);
    }
    out.push_str(value);
    Ok(())
}

pub fn paragraph(text: &str, anchor: usize, caret: usize, column: usize) -> Result<Edit> {
    if !(20..=240).contains(&column) {
        return Err(Error::InvalidArgument);
    }
    get(text, anchor..anchor)?;
    let current = text::line(text, caret)?;
    let content = get(text, current.clone())?;
    let prefix = indent(content);
    if content.len() == prefix.len() {
        return Ok(Edit {
            range: caret..caret,
            insert: String::new(),
            anchor,
            caret,
        });
    }
    let mut range = current;
    while range.start > 0 {
        let previous = text::line(text, range.start - 1)?;
        let line = get(text, previous.clone())?;
        if indent(line) != prefix || line.len() == prefix.len() {
            break;
        }
        range.start = previous.start;
    }
    while range.end < text.len() {
        let next = text::line(text, range.end + 1)?;
        let line = get(text, next.clone())?;
        if indent(line) != prefix || line.len() == prefix.len() {
            break;
        }
        range.end = next.end;
    }
    // Leave the paragraph's last newline outside the replacement.
    reflow(text, range, prefix, column, anchor, caret)
}

fn reflow(
    text: &str,
    range: Range<usize>,
    prefix: &str,
    column: usize,
    anchor: usize,
    caret: usize,
) -> Result<Edit> {
    let source = get(text, range.clone())?;
    let mut out = String::new();
    append(&mut out, prefix)?;
    let mut col = text::column(prefix);
    let mut first = true;
    let mut mapped = [None, None];
    let points = [anchor, caret];
    for (point, result) in points.iter().zip(mapped.iter_mut()) {
        if *point >= range.start && *point < range.end {
            let line = text::line(text, *point)?;
            if *point < line.start + prefix.len() {
                *result = Some(range.start + point.saturating_sub(line.start).min(prefix.len()));
            }
        }
    }
    let mut at = 0usize;
    for part in source.split_inclusive([' ', '\t', '\n']) {
        let word = part.trim_end_matches([' ', '\t', '\n']);
        if !word.is_empty() {
            let width = word.chars().count();
            if !first {
                if col.saturating_add(1).saturating_add(width) > column {
                    append(&mut out, "\n")?;
                    append(&mut out, prefix)?;
                    col = text::column(prefix);
                } else {
                    append(&mut out, " ")?;
                    col += 1;
                }
            }
            let new_start = range.start + out.len();
            let old_start = range.start + at;
            let old_end = old_start + word.len();
            for (point, result) in points.iter().zip(mapped.iter_mut()) {
                if result.is_none() && *point >= range.start && *point < old_end {
                    *result = Some(new_start + point.saturating_sub(old_start));
                }
            }
            append(&mut out, word)?;
            col = col.saturating_add(width);
            first = false;
        }
        at += part.len();
    }
    let map = |point: usize, mapped: Option<usize>| -> usize {
        if point < range.start {
            point
        } else if point >= range.end {
            range.start + out.len() + (point - range.end)
        } else {
            mapped.unwrap_or(range.start + out.len())
        }
    };
    let anchor = map(anchor, mapped.first().copied().flatten());
    let caret = map(caret, mapped.get(1).copied().flatten());
    Ok(Edit {
        range,
        insert: out,
        anchor,
        caret,
    })
}

/// A typed separator and its line wrap are one replacement. The scratch
/// copy covers only the lines intersecting the selection, not the document.
pub fn auto_fill(
    text: &str,
    selection: Range<usize>,
    separator: char,
    column: usize,
) -> Result<Edit> {
    if !matches!(separator, ' ' | '\t') || !(20..=240).contains(&column) {
        return Err(Error::InvalidArgument);
    }
    get(text, selection.clone())?;
    let start = text::line(text, selection.start)?.start;
    let end = text::line(text, selection.end)?.end;
    let mut working = get(text, start..end)?.to_string();
    let local = (selection.start - start)..(selection.end - start);
    // Validated above; subtraction preserves both UTF-8 boundaries.
    working.replace_range(local, &separator.to_string());
    let caret = selection.start - start + 1;
    let row = text::line(&working, caret)?;
    let row_text = get(&working, row.clone())?;
    if text::column(row_text) <= column || indent(row_text).len() == row_text.len() {
        let caret = selection.start + 1;
        return Ok(Edit {
            range: selection,
            insert: separator.to_string(),
            anchor: caret,
            caret,
        });
    }
    // Auto Fill changes break points, not the user's other whitespace.
    let prefix = indent(row_text);
    let mut out = String::new();
    append(&mut out, prefix)?;
    let mut col = text::column(prefix);
    let mut first = true;
    let mut at = prefix.len();
    let mut last_end = at;
    let mut mapped = caret - row.start;
    for part in get(row_text, at..row_text.len())?.split_inclusive([' ', '\t']) {
        let word = part.trim_end_matches([' ', '\t']);
        if !word.is_empty() {
            let gap = get(row_text, last_end..at)?;
            let next_col = gap.bytes().fold(col, |col, b| {
                col.saturating_add(if b == b'\t' {
                    text::TAB_WIDTH - col % text::TAB_WIDTH
                } else {
                    1
                })
            });
            if !first && next_col.saturating_add(word.chars().count()) > column {
                let split = at.checked_sub(1).ok_or(Error::InvalidPosition)?;
                append(&mut out, get(row_text, last_end..split)?)?;
                append(&mut out, "\n")?;
                append(&mut out, prefix)?;
                if caret - row.start > split {
                    mapped += prefix.len();
                }
                col = text::column(prefix);
            } else {
                append(&mut out, gap)?;
                col = next_col;
            }
            append(&mut out, word)?;
            col = col.saturating_add(word.chars().count());
            first = false;
            last_end = at + word.len();
        }
        at += part.len();
    }
    append(&mut out, get(row_text, last_end..row_text.len())?)?;
    working.replace_range(row.clone(), &out);
    let caret = start + row.start + mapped;
    Ok(Edit {
        range: start..end,
        insert: working,
        anchor: caret,
        caret,
    })
}
