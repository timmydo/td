# Pinned leap-second input

The operator approved this external data input for td-mta. It adds no Cargo
or runtime dependency. Preserve the complete source and its public-domain
notice; this file is used only by offline tools and tests.

- Source: https://data.iana.org/time-zones/data/leap-seconds.list
- Bytes: 5065
- SHA-256: `db5a895f16853b03bfc865e8d68f9fc8710ef1740e3400c701cd46a5bbbc3433`
- Updated NTP timestamp: 3992312697
- Expiration NTP timestamp: 4023129600 (2027-06-28 00:00 UTC)

The URL is a rolling upstream location. The checked-in bytes and digest pin
the edition updated at 3992312697 (2026-07-06); later upstream editions
will differ and require an explicit reviewed update.

`tools/leap_generate.rs` verifies the type, length, digest and UTF-8 before
parsing numeric transitions. `examples/leap_generate.rs` prints the exact
Rust table; `tests/leap_generate.rs` proves offline regeneration. The normal
crypto-cargo test command compiles examples. On the current x86-64 GNU host
with the default target directory, capture a candidate from the repository
root:

```text
.td-build-cache/crypto-target/x86_64-unknown-linux-gnu/debug/examples/leap_generate td-mta/leap-seconds > td-mta/leap-seconds/dates.rs.candidate
```

Check success and review the candidate before replacing
`src/header_date/leap_dates.rs`. Redirection truncates the candidate before
verification; failure can leave it empty or partial. Keep it outside `src`.
The generator emits already formatted source; tests compare output byte for
byte. Normal tests need no network or external tool.

The 1972-01-01 TAI-UTC value 10 is a baseline, not an insertion. Each of the
27 later +1 transitions identifies the preceding UTC day's 23:59:60. The
last such date is 2016-12-31. Numeric timestamps, not comments, determine
the generated dates. Ordering, midnight/month boundaries, baseline, offset
increments and update/expiration ordering are checked. Update metadata must
not precede the baseline; both update and final transition precede expiry.
Announcements may precede the effective transition. Negative or other
unexpected offset changes refuse generation and require a reviewed extension.

The expiration timestamp is provenance/freshness metadata. It neither erases
historical insertions nor proves that no new announcement could precede that
date. Runtime qualification recognizes listed positive insertions; all other
second-60 values remain unverified. Ordinary seconds use the existing
calendar/offset checks. Future data updates are reviewed git changes, with
explicit source size/digest and regenerated table, never automatic fetches.
