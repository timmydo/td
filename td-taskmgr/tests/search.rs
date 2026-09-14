use td_taskmgr::search::Search;
#[test]
fn edits_multibyte_text_at_character_boundaries_and_caps_growth() {
    let mut search = Search::default();
    for ch in ["a", "é", "中", "z"] {
        assert!(search.key(ch));
    }
    assert_eq!(search.text(), "aé中z");
    search.key("Left");
    search.key("Backspace");
    assert_eq!(search.text(), "aéz");
    search.key("Home");
    search.key("Delete");
    assert_eq!(search.text(), "éz");
    search.key("End");
    search.key("C-a");
    search.key("中");
    assert_eq!(search.text(), "中");
    search.key("C-a");
    search.key("Backspace");
    for _ in 0..256 {
        search.key("x");
    }
    assert_eq!(search.text().len(), 256);
    assert!(!search.key("é"));
    assert_eq!(search.text().len(), 256);
    search.place(0);
    search.key("Delete");
    assert_eq!(search.text().len(), 255);
    assert!(!search.key("é"));
}
