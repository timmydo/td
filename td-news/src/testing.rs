pub fn tempdir() -> std::io::Result<td_fs::TempDir> {
    td_fs::TempDir::new("td-news")
}
