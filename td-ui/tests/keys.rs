//! The key list: its lines, its scrolling and its panel.

use td_ui::keys::{self, Help, Line, Panel, Section, Step};
use td_ui::raster::{Draw, Primitive, Scale, Surface, ACCENT, SELECTED};

fn sections() -> Vec<Section> {
    vec![
        Section::new("List", &[("j/k", "move"), ("Enter", "open")]),
        Section::new("Article", &[("Space", "page down"), ("", "and on")]),
        keys::window(),
    ]
}

#[test]
fn lines_title_each_section_pad_the_keys_and_end_with_the_windows() {
    let lines = keys::lines(&sections(), usize::MAX);
    let text: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
    assert_eq!(
        text,
        [
            "List",
            "  j/k    move",
            "  Enter  open",
            "",
            "Article",
            "  Space  page down",
            "         and on",
            "",
            "Window",
            "  F1     show or hide this list of keys",
            "  F12    next colour theme, kept for this program",
        ]
    );
    let titles: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.title)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(titles, [0, 4, 8]);
    // A keys cell past the column's ceiling runs on rather than widening
    // every row.
    let long = "x".repeat(keys::MAX_KEYS_COLUMN + 6);
    let leaked: &'static str = Box::leak(long.into_boxed_str());
    let lines = keys::lines(
        &[Section::new("Wide", &[(leaked, "w"), ("a", "b")])],
        usize::MAX,
    );
    assert_eq!(
        lines[2],
        Line {
            text: format!("  a{}  b", " ".repeat(keys::MAX_KEYS_COLUMN - 1)),
            title: false
        }
    );
}

#[test]
fn the_reading_keys_scroll_within_the_lines_and_the_closing_keys_close() {
    let mut help = Help::default();
    assert!(!help.is_open());
    help.open();
    assert!(help.is_open());
    // Twenty lines, five shown: the last page starts at fifteen.
    // A reading key held at an end moves nothing and is only kept.
    let (moved, kept) = (Step::Moved, Step::Kept);
    for (chord, first, step) in [
        ("j", 1, moved),
        ("Down", 2, moved),
        ("k", 1, moved),
        ("PageDown", 6, moved),
        (" ", 11, moved),
        (" ", 15, moved),
        ("j", 15, kept),
        ("End", 15, kept),
        ("PageUp", 10, moved),
        ("Home", 0, moved),
        ("Up", 0, kept),
        ("g", 0, kept),
        ("G", 15, moved),
        ("g", 0, moved),
        ("End", 15, moved),
    ] {
        assert_eq!(help.key(chord, 20, 5), step, "{chord}");
        assert_eq!(help.first(), first, "{chord}");
    }
    assert_eq!(help.key("x", 20, 5), Step::Kept);
    assert_eq!(
        help.key("Space", 20, 5),
        Step::Kept,
        "the keymap says \" \""
    );
    assert_eq!(help.key("C-j", 20, 5), Step::Kept);
    assert_eq!(help.first(), 15);
    help.wheel(-3, 20, 5);
    assert_eq!(help.first(), 12);
    help.wheel(9, 20, 5);
    assert_eq!(help.first(), 15);
    for chord in ["Escape", "q", "?", keys::CHORD] {
        help.open();
        assert_eq!(help.first(), 0, "opened from the top");
        assert_eq!(help.key(chord, 20, 5), Step::Closed);
        assert!(!help.is_open());
    }
    // A taller page, or fewer lines, holds the last line on the last row.
    help.key("End", 20, 5);
    help.clamp(20, 10);
    assert_eq!(help.first(), 10);
    help.clamp(4, 10);
    assert_eq!(help.first(), 0);
    // Fewer lines than rows: nothing to scroll.
    help.open();
    assert_eq!(help.key("End", 3, 5), Step::Kept);
    assert_eq!(help.first(), 0);
}

#[test]
fn a_description_wider_than_the_columns_wraps_under_itself() {
    let rows = [
        ("j/k", "move the selection up and down the list"),
        ("Enter", "open"),
        ("x", "supercalifragilisticexpialidocious"),
    ];
    let text: Vec<String> = keys::lines(&[Section::new("List", &rows)], 24)
        .into_iter()
        .map(|line| line.text)
        .collect();
    assert_eq!(
        text,
        [
            "List",
            "  j/k    move the",
            "         selection up and",
            "         down the list",
            "  Enter  open",
            "  x      supercalifragili",
            "         sticexpialidocio",
            "         us",
        ]
    );
    // Wider, the room left beside the keys decides: 30 less the 9-cell
    // head is 21.
    let text: Vec<String> = keys::lines(&[Section::new("List", &rows)], 30)
        .into_iter()
        .map(|line| line.text)
        .collect();
    assert_eq!(
        &text[1..3],
        [
            "  j/k    move the selection up",
            "         and down the list"
        ]
    );
    assert!(text.iter().all(|line| line.chars().count() <= 30));
    // However narrow, a description keeps MIN_WRAP.
    let narrow = keys::lines(&[Section::new("List", &rows)], 4);
    assert!(narrow
        .iter()
        .all(|line| line.text.chars().count() <= 9 + keys::MIN_WRAP));
}

#[test]
fn the_title_hint_names_the_chord() {
    assert!(keys::TITLE_HINT.starts_with(keys::CHORD));
}

#[test]
fn the_panel_sits_inside_the_margin_and_paints_its_title_and_section_titles() {
    let surface = Surface::new(1200, 400, Scale::new(1).unwrap()).unwrap();
    let panel = Panel::new(surface).unwrap();
    let margin = (keys::MARGIN * td_ui::CELL_WIDTH) as i64;
    let width = (keys::MAX_COLUMNS * td_ui::CELL_WIDTH) as u32;
    assert_eq!(panel.frame.width, width, "at most the widest extent");
    assert_eq!(panel.frame.x, (1200 - i64::from(width)) / 2, "centred");
    assert_eq!(panel.frame.y, margin);
    assert_eq!(panel.frame.height, 400 - 2 * margin as u32);
    assert!(panel.page() > 0);
    let lines = keys::lines(&sections(), panel.columns(surface));
    assert!(lines
        .iter()
        .all(|line| line.text.chars().count() <= panel.columns(surface)));
    let mut help = Help::default();
    help.open();
    let mut draws: Vec<Draw> = Vec::new();
    panel.emit(surface, &help, &lines, surface.bounds(), &mut |draw| {
        draws.push(draw)
    });
    assert!(draws.iter().any(|draw| matches!(
        draw.primitive,
        Primitive::Fill { rect, color: SELECTED } if rect == panel.title
    )));
    // Three section titles in the accent ink.
    let accented = draws
        .iter()
        .filter(|draw| match draw.primitive {
            Primitive::Glyph { style, .. } => style.ink == ACCENT,
            _ => false,
        })
        .count();
    assert_eq!(accented, "List".len() + "Article".len() + "Window".len());
    // Nothing paints outside the frame.
    assert!(draws
        .iter()
        .all(|draw| draw.clip.intersection(panel.frame) == Some(draw.clip)));
    // The hint is left out where it would meet the title.
    let glyphs = |draws: &[Draw]| {
        draws
            .iter()
            .filter(|draw| matches!(draw.primitive, Primitive::Glyph { style, .. } if style.background == SELECTED))
            .count()
    };
    assert_eq!(
        glyphs(&draws),
        "Keys".len() + keys::TITLE_HINT.chars().count()
    );
    let slim = Surface::new(220, 400, Scale::new(1).unwrap()).unwrap();
    let panel = Panel::new(slim).unwrap();
    let mut draws: Vec<Draw> = Vec::new();
    panel.emit(slim, &help, &lines, slim.bounds(), &mut |draw| {
        draws.push(draw)
    });
    assert_eq!(glyphs(&draws), "Keys".len());
    // A surface with no room for a row has no panel.
    let small = Surface::new(200, 60, Scale::new(1).unwrap()).unwrap();
    assert_eq!(Panel::new(small), None);
    // Narrower than the widest extent, it takes the width less the margin.
    let narrow = Surface::new(400, 300, Scale::new(2).unwrap()).unwrap();
    let panel = Panel::new(narrow).unwrap();
    assert_eq!(i64::from(panel.frame.width), 400 - 4 * margin);
}

/// The overlay a window keeps: opened with the program's sections and
/// the window's, laid out for its surface and again for another, scrolled
/// by key and wheel, its first line held to the page when it paints, and
/// emptied when it closes.
#[test]
fn the_overlay_opens_lays_out_scrolls_paints_and_closes() {
    use td_ui::keys::Overlay;
    let wide = Surface::new(1200, 300, Scale::new(1).unwrap()).unwrap();
    let narrow = Surface::new(300, 300, Scale::new(1).unwrap()).unwrap();
    let rows = vec![("x", "a description long enough to wrap on a narrow surface"); 20];
    let mut overlay = Overlay::default();
    assert!(!overlay.is_open());
    let mut draws = 0;
    overlay.emit(wide, wide.bounds(), &mut |_| draws += 1);
    assert_eq!(draws, 0, "closed, nothing painted");
    overlay.open(vec![Section::new("Program", &rows)], wide);
    assert!(overlay.is_open());
    let wide_lines = overlay.lines().len();
    assert!(overlay
        .lines()
        .iter()
        .any(|l| l.title && l.text == "Window"));
    overlay.lay_out(narrow);
    assert!(overlay.lines().len() > wide_lines, "wrapped");
    assert_eq!(overlay.key("End", narrow), Step::Moved);
    let end = overlay.help().first();
    overlay.wheel(-2, narrow);
    assert_eq!(overlay.help().first(), end - 2);
    assert_eq!(overlay.key("x", narrow), Step::Kept);
    overlay.key("End", narrow);
    let narrow_end = overlay.help().first();
    overlay.lay_out(wide);
    overlay.emit(wide, wide.bounds(), &mut |_| draws += 1);
    assert!(draws > 0);
    assert!(overlay.help().first() < narrow_end, "held to the wide page");
    assert_eq!(overlay.key("Escape", wide), Step::Closed);
    assert_eq!(overlay, td_ui::keys::Overlay::default(), "emptied");
    // Closed, it keeps nothing and moves nothing.
    assert_eq!(overlay.key("q", wide), Step::Kept);
    assert_eq!(overlay.key("j", wide), Step::Kept);
    overlay.wheel(3, wide);
    assert_eq!(overlay, td_ui::keys::Overlay::default());
    // Opened again, from the top, with one window section.
    overlay.open(vec![Section::new("Program", &rows)], narrow);
    overlay.key("End", narrow);
    overlay.open(vec![Section::new("Program", &rows)], narrow);
    assert_eq!(overlay.help().first(), 0);
    let windows = overlay
        .lines()
        .iter()
        .filter(|l| l.title && l.text == "Window")
        .count();
    assert_eq!(windows, 1);
    // A resize holds the first line to the new page before any paint.
    overlay.key("End", narrow);
    let narrow_end = overlay.help().first();
    overlay.lay_out(wide);
    assert!(overlay.help().first() < narrow_end);
    // On a surface with no room it paints nothing but keeps its keys.
    let tiny = Surface::new(120, 40, Scale::new(1).unwrap()).unwrap();
    overlay.lay_out(tiny);
    let mut drawn = 0;
    overlay.emit(tiny, tiny.bounds(), &mut |_| drawn += 1);
    assert_eq!(drawn, 0);
    assert_eq!(overlay.key("j", tiny), Step::Moved);
    overlay.close();
    assert!(!overlay.is_open());
}
