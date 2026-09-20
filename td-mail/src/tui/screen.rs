//! The views' drawing surface: the cursor-and-attribute writer they were
//! written against, over td-ui's cell screen rather than a terminal. A
//! view moves the cursor, sets an attribute and writes text, as it wrote
//! escape sequences; here each call lands on the cells of the window's
//! `Screen`, which the window presents once the view has rendered. A
//! write past the right edge is clipped, where the terminal wrapped to
//! the next row; every view writes within the columns it is given.

use crate::config::Theme;
use std::io;
use td_ui::screen::{self, Screen, Style};

fn rgb((r, g, b): (u8, u8, u8)) -> u32 {
    u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)
}

/// The base style the theme names: its foreground and background over the
/// toolkit's ink and paper where the theme leaves one unset.
pub fn base_style(theme: &Theme) -> Style {
    Style::new(
        theme.fg.map_or(screen::INK, rgb),
        theme.bg.map_or(screen::PAPER, rgb),
    )
}

/// One render's writer over the window's screen. The cursor is a cell,
/// the attribute a `Style`, and a write past the right edge is clipped,
/// the cursor left at the edge.
pub struct Terminal<'s> {
    screen: &'s mut Screen,
    pub rows: u16,
    pub cols: u16,
    theme: Theme,
    base: Style,
    style: Style,
    in_selection: bool,
    /// The cursor, zero-based; `move_to` takes the terminal's one-based
    /// row and column.
    row: usize,
    col: usize,
}

impl<'s> Terminal<'s> {
    pub fn new(screen: &'s mut Screen, theme: Theme) -> Self {
        let base = base_style(&theme);
        let rows = u16::try_from(screen.rows()).unwrap_or(u16::MAX);
        let cols = u16::try_from(screen.columns()).unwrap_or(u16::MAX);
        Terminal {
            screen,
            rows,
            cols,
            theme,
            base,
            style: base,
            in_selection: false,
            row: 0,
            col: 0,
        }
    }

    /// The base over every cell, the cursor home and the attribute the
    /// base, so a render starts from nothing its predecessor set.
    pub fn clear(&mut self) -> io::Result<()> {
        self.screen.clear(self.base);
        self.row = 0;
        self.col = 0;
        self.style = self.base;
        self.in_selection = false;
        Ok(())
    }

    pub fn move_to(&mut self, row: u16, col: u16) -> io::Result<()> {
        self.row = usize::from(row.saturating_sub(1));
        self.col = usize::from(col.saturating_sub(1));
        Ok(())
    }

    #[allow(dead_code)]
    pub fn clear_line(&mut self) -> io::Result<()> {
        self.screen.clear_row(self.row, self.col, self.style);
        Ok(())
    }

    /// Writes at the cursor and advances it. A tab moves to the next stop
    /// of eight cells and an escape sequence takes no cell, as the terminal
    /// had them, since the screen would draw either control scalar as the
    /// replacement character: an SGR sequence sets the attribute, which is
    /// how the HTML rendering marks its spans, and any other sequence is
    /// dropped.
    pub fn write_str(&mut self, s: &str) -> io::Result<()> {
        if !s.contains(['\t', '\x1b']) {
            let written = self.screen.write(self.row, self.col, s, self.style);
            self.col = self.col.saturating_add(written);
            return Ok(());
        }
        // Past the edge a put lands nowhere and the cursor stays there, as
        // the whole-string write leaves it; the sequences still apply, so
        // a clipped span's closing reset is not lost.
        let mut chars = s.chars();
        while let Some(ch) = chars.next() {
            match ch {
                '\t' => {
                    let stop = (self.col / 8).saturating_add(1).saturating_mul(8);
                    while self.col < stop && self.screen.put(self.row, self.col, ' ', self.style)
                    {
                        self.col = self.col.saturating_add(1);
                    }
                }
                '\x1b' => self.escape(&mut chars),
                _ => {
                    if self.screen.put(self.row, self.col, ch, self.style) {
                        self.col = self.col.saturating_add(1);
                    }
                }
            }
        }
        Ok(())
    }

    /// Consumes the sequence an escape introduces: a control sequence
    /// through its final byte, applied when it selects graphic rendition;
    /// any other escape takes the one scalar that names it.
    fn escape(&mut self, chars: &mut std::str::Chars<'_>) {
        if chars.next() != Some('[') {
            return;
        }
        let mut parameters = String::new();
        for ch in chars.by_ref() {
            if ('\x40'..='\x7e').contains(&ch) {
                if ch == 'm' {
                    self.select_graphic_rendition(&parameters);
                }
                return;
            }
            parameters.push(ch);
        }
    }

    /// The SGR subset the HTML rendering emits: the weight and its reset,
    /// a direct colour for the ink or the background and their defaults,
    /// which are the base's. Italic, strike-through and faint have no
    /// cell attribute and pass; an unknown parameter is skipped.
    fn select_graphic_rendition(&mut self, parameters: &str) {
        // A private sequence is another protocol's, not a rendition.
        if parameters.starts_with(['<', '=', '>', '?']) {
            return;
        }
        // No parameter is the reset, as the terminal read `ESC [ m`.
        let parameters = if parameters.is_empty() { "0" } else { parameters };
        let mut codes = parameters.split(';').map(|code| code.parse::<u8>().ok());
        while let Some(code) = codes.next() {
            match code {
                Some(0) => self.style = self.base,
                Some(1) => self.style = self.style.bold(),
                Some(22) => self.style.weight = self.base.weight,
                Some(39) => self.style.ink = self.base.ink,
                Some(49) => self.style.background = self.base.background,
                Some(select @ (38 | 48)) => {
                    // Only the direct form; an indexed colour ends the
                    // sequence's meaning here.
                    if codes.next() != Some(Some(2)) {
                        return;
                    }
                    let (Some(Some(r)), Some(Some(g)), Some(Some(b))) =
                        (codes.next(), codes.next(), codes.next())
                    else {
                        return;
                    };
                    if select == 38 {
                        self.style.ink = rgb((r, g, b));
                    } else {
                        self.style.background = rgb((r, g, b));
                    }
                }
                _ => {}
            }
        }
    }

    pub fn set_reverse(&mut self) -> io::Result<()> {
        self.style = self.style.reversed();
        Ok(())
    }

    #[allow(dead_code)]
    pub fn set_bold(&mut self) -> io::Result<()> {
        self.style = self.style.bold();
        Ok(())
    }

    /// Back to the theme's base colours, as the reset re-applied them.
    pub fn reset_attr(&mut self) -> io::Result<()> {
        self.style = self.base;
        self.in_selection = false;
        Ok(())
    }

    fn apply(&mut self, fg: Option<(u8, u8, u8)>, bg: Option<(u8, u8, u8)>) {
        if let Some(bg) = bg {
            self.style.background = rgb(bg);
        }
        if let Some(fg) = fg {
            self.style.ink = rgb(fg);
        }
    }

    /// Apply selection colors (for highlighted/cursor rows).
    /// Falls back to reverse video if no theme colors are set.
    pub fn set_selection(&mut self) -> io::Result<()> {
        self.in_selection = true;
        if self.theme.selection_bg.is_some() || self.theme.selection_fg.is_some() {
            self.apply(self.theme.selection_fg, self.theme.selection_bg);
            Ok(())
        } else {
            self.set_reverse()
        }
    }

    /// Apply status bar colors.
    /// Falls back to reverse video if no theme colors are set.
    pub fn set_status(&mut self) -> io::Result<()> {
        if self.theme.status_bg.is_some() || self.theme.status_fg.is_some() {
            self.apply(self.theme.status_fg, self.theme.status_bg);
            Ok(())
        } else {
            self.set_reverse()
        }
    }

    /// Apply header colors (bold + header_fg if set).
    pub fn set_header(&mut self) -> io::Result<()> {
        self.style = self.style.bold();
        self.apply(self.theme.header_fg, None);
        Ok(())
    }

    /// Apply bold text colors (for unread items).
    /// Uses bold_fg if set, otherwise plain bold.
    /// When inside a selection, only adds bold without changing fg,
    /// so that selection_fg takes priority for contrast.
    pub fn set_bold_text(&mut self) -> io::Result<()> {
        self.style = self.style.bold();
        if !self.in_selection {
            self.apply(self.theme.bold_fg, None);
        }
        Ok(())
    }

    /// The terminal's buffer flushed at the end of a render; the screen is
    /// presented by the window once the render returns.
    pub fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Write a string truncated to fit within `max_width` columns.
    pub fn write_truncated(&mut self, s: &str, max_width: u16) -> io::Result<()> {
        let max = usize::from(max_width);
        if s.len() <= max {
            return self.write_str(s);
        }
        // Truncate at a char boundary, counting bytes as the terminal
        // writer did, so a view's own width arithmetic is unchanged.
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        let head = s.get(..end).unwrap_or_default();
        self.write_str(head)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use td_ui::raster::{Scale, Surface};

    fn screen() -> Screen {
        let surface = Surface::new(8 * 40, 16 * 6, Scale::default()).unwrap();
        Screen::new(surface, Style::default()).unwrap()
    }

    #[test]
    fn writes_land_on_the_cells_the_cursor_names_and_clip_at_the_edge() {
        let mut screen = screen();
        let mut term = Terminal::new(&mut screen, Theme::default());
        assert_eq!((term.rows, term.cols), (6, 40));
        term.clear().unwrap();
        term.move_to(2, 3).unwrap();
        term.write_str("ab").unwrap();
        term.write_str("c").unwrap();
        term.move_to(6, 39).unwrap();
        term.write_truncated("wide", 10).unwrap();
        term.move_to(1, 1).unwrap();
        term.write_truncated("héllo", 2).unwrap();
        // A tab lands on the next stop of eight from the cursor's column.
        term.move_to(4, 3).unwrap();
        term.write_str("a\tb\t\tc").unwrap();
        assert_eq!(screen.line(3).unwrap(), "  a     b               c");
        assert_eq!(screen.line(1).unwrap(), "  abc");
        assert_eq!(screen.line(5).unwrap(), format!("{:38}wi", ""));
        assert_eq!(screen.line(0).unwrap(), "h");
        assert!(screen.line(2).unwrap().is_empty());
    }

    /// The HTML rendering's SGR sequences set the attribute and take no
    /// cell; the attributes the toolkit lacks and any other sequence pass.
    #[test]
    fn escape_sequences_set_the_attribute_and_take_no_cells() {
        let mut screen = screen();
        let mut term = Terminal::new(&mut screen, Theme::default());
        term.clear().unwrap();
        term.move_to(1, 1).unwrap();
        term.write_str("a\x1b[1mb\x1b[22mc\x1b[3m\x1b[9m\x1b[2md\x1b[23;29;22me")
            .unwrap();
        term.write_str("\x1b[38;2;1;2;3mf\x1b[48;2;4;5;6mg\x1b[39mh\x1b[49mi")
            .unwrap();
        // Not SGR, not a control sequence, an indexed colour, a private
        // sequence (not a rendition, whatever follows its marker), a
        // parameter that is not a number (skipped, the next one read),
        // the bare reset, and an unterminated sequence.
        term.write_str("\x1b[2Kj\x1bMk\x1b[38;5;9ml\x1b[?;1mm\x1b[:;1mn\x1b[mo\x1b[1")
            .unwrap();
        assert_eq!(screen.line(0).unwrap(), "abcdefghijklmno");
        let style = |col: usize| screen.cell(0, col).unwrap().style;
        let base = Style::default();
        assert_eq!(style(0), base);
        assert_eq!(style(1), base.bold());
        assert_eq!(style(2), base);
        assert_eq!(style(3), base);
        assert_eq!(style(4), base);
        assert_eq!(style(5), Style::new(0x010203, screen::PAPER));
        assert_eq!(style(6), Style::new(0x010203, 0x040506));
        assert_eq!(style(7), Style::new(screen::INK, 0x040506));
        assert_eq!(style(8), base);
        assert_eq!(style(9), base);
        assert_eq!(style(10), base);
        assert_eq!(style(11), base);
        assert_eq!(style(12), base);
        assert_eq!(style(13), base.bold());
        assert_eq!(style(14), base);
        // The base a theme sets is what the defaults return to.
        let theme = Theme {
            fg: Some((4, 5, 6)),
            ..Theme::default()
        };
        let mut term = Terminal::new(&mut screen, theme);
        term.move_to(2, 1).unwrap();
        term.write_str("\x1b[38;2;7;8;9mn\x1b[39mo").unwrap();
        // A span clipped at the edge still closes, and the cursor stays at
        // the edge; a clear resets the attribute with the cells.
        term.move_to(3, 39).unwrap();
        term.write_str("\x1b[1mab\tc\x1b[22md").unwrap();
        term.write_str("e").unwrap();
        term.move_to(4, 1).unwrap();
        term.write_str("f").unwrap();
        assert_eq!(screen.cell(1, 0).unwrap().style.ink, 0x070809);
        assert_eq!(screen.cell(1, 1).unwrap().style.ink, 0x040506);
        assert_eq!(screen.line(2).unwrap(), format!("{:38}ab", ""));
        assert_eq!(screen.cell(2, 39).unwrap().style.weight, Style::default().bold().weight);
        assert_eq!(screen.cell(3, 0).unwrap().style.weight, Style::default().weight);
        let mut term = Terminal::new(&mut screen, Theme::default());
        term.set_bold_text().unwrap();
        term.set_selection().unwrap();
        term.clear().unwrap();
        term.write_str("g").unwrap();
        assert_eq!(screen.cell(0, 0).unwrap().style, Style::default());
    }

    #[test]
    fn attributes_follow_the_theme_and_reset_to_its_base() {
        let mut screen = screen();
        let theme = Theme {
            bg: Some((1, 2, 3)),
            fg: Some((4, 5, 6)),
            bold_fg: Some((7, 8, 9)),
            selection_bg: Some((10, 11, 12)),
            selection_fg: None,
            status_bg: None,
            status_fg: Some((13, 14, 15)),
            header_fg: None,
        };
        let base = base_style(&theme);
        assert_eq!(base, Style::new(0x040506, 0x010203));
        let mut term = Terminal::new(&mut screen, theme);
        term.clear().unwrap();
        assert_eq!(screen.ground(), 0x010203);
        let mut term = Terminal::new(&mut screen, Theme::default());
        term.clear().unwrap();
        term.set_selection().unwrap();
        term.set_bold_text().unwrap();
        term.move_to(1, 1).unwrap();
        term.write_str("s").unwrap();
        term.reset_attr().unwrap();
        term.set_status().unwrap();
        term.move_to(2, 1).unwrap();
        term.write_str("t").unwrap();
        term.reset_attr().unwrap();
        term.set_header().unwrap();
        term.move_to(3, 1).unwrap();
        term.write_str("h").unwrap();
        term.reset_attr().unwrap();
        term.move_to(4, 1).unwrap();
        term.write_str("p").unwrap();
        // No theme: selection and status are the base reversed, bold text
        // inside the selection keeps the selection's ink.
        let selected = screen.cell(0, 0).unwrap().style;
        assert_eq!(selected, Style::default().reversed().bold());
        assert_eq!(screen.cell(1, 0).unwrap().style, Style::default().reversed());
        assert_eq!(screen.cell(2, 0).unwrap().style, Style::default().bold());
        assert_eq!(screen.cell(3, 0).unwrap().style, Style::default());
    }

    #[test]
    fn themed_attributes_apply_the_colours_the_theme_sets() {
        let mut screen = screen();
        let theme = Theme {
            bg: Some((1, 2, 3)),
            fg: Some((4, 5, 6)),
            bold_fg: Some((7, 8, 9)),
            selection_bg: Some((10, 11, 12)),
            selection_fg: None,
            status_bg: None,
            status_fg: Some((13, 14, 15)),
            header_fg: Some((16, 17, 18)),
        };
        let mut term = Terminal::new(&mut screen, theme);
        term.clear().unwrap();
        term.set_selection().unwrap();
        term.set_bold_text().unwrap();
        term.move_to(1, 1).unwrap();
        term.write_str("s").unwrap();
        term.reset_attr().unwrap();
        term.set_bold_text().unwrap();
        term.move_to(2, 1).unwrap();
        term.write_str("b").unwrap();
        term.reset_attr().unwrap();
        term.set_status().unwrap();
        term.move_to(3, 1).unwrap();
        term.write_str("t").unwrap();
        term.reset_attr().unwrap();
        term.set_header().unwrap();
        term.move_to(4, 1).unwrap();
        term.write_str("h").unwrap();
        // Selection keeps the base ink under bold: the theme's bold colour
        // yields to the selection's contrast, as the terminal's did.
        assert_eq!(
            screen.cell(0, 0).unwrap().style,
            Style::new(0x040506, 0x0a0b0c).bold()
        );
        assert_eq!(
            screen.cell(1, 0).unwrap().style,
            Style::new(0x070809, 0x010203).bold()
        );
        assert_eq!(screen.cell(2, 0).unwrap().style, Style::new(0x0d0e0f, 0x010203));
        assert_eq!(
            screen.cell(3, 0).unwrap().style,
            Style::new(0x101112, 0x010203).bold()
        );
    }
}
