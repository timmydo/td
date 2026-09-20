//! The COMPILED local-source roster (re #469 local-source-roster split):
//! every catalog `local_source` recipe's key, main path, and sibling
//! `local_source_trees`, compiled into both td-recipe-eval and td-builder
//! from the same audited repo file.
//!
//! Unlike `seed/seed-digests.txt`, `seed/local-source-roster.txt` pins no
//! content hash. A local source's bytes are the checkout itself, so both
//! sides RE-DERIVE its identity live, on every run, straight from the
//! declared paths (`ensure_local_source` here; `auto_seed_provenance` and
//! `authenticate_seed_db` in td-builder) — there is nothing to compare a
//! committed hash against that would not immediately go stale on the next
//! ordinary source edit. This table only records the DECLARATION: which key
//! stages which paths. It changes only when a recipe adds, removes, or
//! renames a `local_source`/`local_source_trees` declaration — not on every
//! edit inside the staged trees, which is the churn #469 originally caused
//! (105 commits touched `seed/seed-digests.txt`; the `td-portal-source` row
//! alone was rewritten 29 times).
//!
//! Regenerate with `td-recipe-eval local-source-roster > \
//! seed/local-source-roster.txt` whenever a recipe's `local_source` or
//! `local_source_trees` declaration changes; `local-source-roster --check`
//! (the `local-source-roster` preflight) reds otherwise.

const TABLE: &str = include_str!("../../../../seed/local-source-roster.txt");

/// One roster row: the key, its declared main path, and its sibling trees in
/// declared order (empty when the recipe stages no siblings).
pub(crate) type Row<'a> = (&'a str, &'a str, Vec<&'a str>);

/// Parse a local-source-roster table. Delegates to `td_engine::local_source`
/// (re #469 blocker-review follow-up): the parser used to be duplicated
/// verbatim between this binary and td-builder, which is exactly the kind of
/// divergence risk the shared-crate design was supposed to rule out for the
/// staging/exclusion logic. Moving the parser there too means there is only
/// ONE place that decides what a roster row means, on both sides.
pub(crate) fn parse(text: &str) -> Result<Vec<Row<'_>>, String> {
    td_engine::local_source::parse_roster(text)
}

/// The compiled rows.
pub(crate) fn rows() -> Result<Vec<Row<'static>>, String> {
    parse(TABLE)
}

/// The compiled declaration for a local-source key, if the roster names one:
/// its main path and sibling trees.
pub(crate) fn expected(key: &str) -> Result<Option<(&'static str, Vec<&'static str>)>, String> {
    Ok(rows()?
        .into_iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, path, trees)| (path, trees)))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The parser's own behavior (accepted shapes, rejected garbage, the
    // empty-sibling-element case) is tested ONCE, in
    // `td_engine::local_source::tests` — this side only owns the
    // include_str! and needs a thin sanity check that ITS embedded table
    // parses through the shared parser.
    #[test]
    fn compiled_table_parses() {
        // Cold-safe: only checks the compiled table is well-formed and
        // duplicate-free, not that it agrees with the catalog (the coverage
        // test in check_runner.rs does that, and needs the catalog walk).
        rows().unwrap();
    }
}
