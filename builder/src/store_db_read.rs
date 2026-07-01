//! A minimal, zero-dependency *reader* for the guix/Nix store SQLite database —
//! the inverse of `store_db` (the writer). td now WRITES its store DB (the
//! daemon's `ValidPaths`/`Refs`/`DerivationOutputs` authority) and READS it back
//! itself, with neither the guix-daemon nor the `sqlite3` engine in td's own
//! store-query path (td-store-db track: "own the store, then diverge").
//!
//! Scope: parse the SQLite file format `store_db` produces — the 100-byte header,
//! the `sqlite_master` schema b-tree (to map a table name to its rootpage), and a
//! table b-tree (leaf pages `0x0d`, descending interior pages `0x05`) into its
//! rows. Records are decoded by the same serial-type/varint rules the writer uses.
//! Overflow pages and index b-trees are not needed for the store tables td writes
//! (short rows; no indexes) and are rejected rather than mis-read.
//!
//! Format reference: the SQLite "Database File Format"
//! (https://www.sqlite.org/fileformat2.html). All multi-byte integers are
//! big-endian; lengths are SQLite varints (big-endian base-128, high bit =
//! continue, up to 9 bytes — the 9th byte contributes a full 8 bits).

use std::collections::{HashMap, HashSet};

const HEADER_LEN: usize = 100;
const LEAF: u8 = 0x0d; // table b-tree leaf
const INTERIOR: u8 = 0x05; // table b-tree interior

/// A decoded column value. Mirrors `store_db::Value`, plus `Blob` for
/// completeness of the record format (the store schema does not use it).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Text(String),
    Blob(Vec<u8>),
}

/// An opened store DB: the raw bytes plus the parsed page size.
pub struct Db {
    data: Vec<u8>,
    page_size: usize,
}

impl Db {
    /// Validate the file header and read the page size.
    pub fn open(data: Vec<u8>) -> Result<Db, String> {
        if data.len() < HEADER_LEN || &data[0..16] != b"SQLite format 3\0" {
            return Err("not a SQLite 3 database (bad magic)".to_string());
        }
        // Page size: bytes 16..18 big-endian; the value 1 means 65536.
        let raw = u16::from_be_bytes([data[16], data[17]]);
        let page_size = if raw == 1 { 65536 } else { raw as usize };
        if page_size < 512 || page_size & (page_size - 1) != 0 {
            return Err(format!("invalid page size {page_size}"));
        }
        // Text encoding (bytes 56..60): we only handle UTF-8 (1), as the writer emits.
        let enc = u32::from_be_bytes([data[56], data[57], data[58], data[59]]);
        if enc != 0 && enc != 1 {
            return Err(format!("unsupported text encoding {enc} (only UTF-8)"));
        }
        Ok(Db { data, page_size })
    }

    /// The rows of a table by name: `(rowid, column values)` in b-tree order
    /// (ascending rowid). The rowid is the value of an `integer primary key`
    /// alias column (stored as NULL in the record), so callers resolving such a
    /// column use the rowid. Mirrors the `(rowid, Vec<Value>)` shape the writer
    /// takes, so a write→read round-trip is the identity on rows.
    pub fn table(&self, name: &str) -> Result<Vec<(i64, Vec<Value>)>, String> {
        let rootpage = self.rootpage_of(name)?;
        let mut rows = Vec::new();
        self.walk_table(rootpage, &mut rows)?;
        Ok(rows)
    }

    /// The set of store paths reachable from `root` over the `Refs` graph — the
    /// GC-reachable closure (the daemon's GC "mark" set; `guix gc -R root`),
    /// `root` included. Errors if `root` is not a `ValidPaths` entry. Resolves
    /// `Refs(referrer, reference)` ids to paths via the `ValidPaths` rowid.
    pub fn closure(&self, root: &str) -> Result<Vec<String>, String> {
        self.closure_roots(std::slice::from_ref(&root.to_string()))
    }

    /// The UNION of the GC-reachable closures of every path in `roots` (each root
    /// included), sorted and deduped — the daemon's `guix gc --requisites root…`
    /// over the same `Refs` graph. Errors if ANY root is not a `ValidPaths` entry
    /// (symmetric with single-root `closure`). The `ValidPaths`/`Refs` tables are
    /// parsed ONCE and the BFS seeds from every root, so a many-root query over a
    /// full `/var/guix/db` costs one DB parse, not one per root. An empty `roots`
    /// yields an empty closure.
    pub fn closure_roots(&self, roots: &[String]) -> Result<Vec<String>, String> {
        let mut path_of: HashMap<i64, String> = HashMap::new();
        let mut id_of: HashMap<String, i64> = HashMap::new();
        for (rowid, cols) in self.table("ValidPaths")? {
            if let Some(Value::Text(p)) = cols.get(1) {
                path_of.insert(rowid, p.clone());
                id_of.insert(p.clone(), rowid);
            }
        }
        let mut edges: HashMap<i64, Vec<i64>> = HashMap::new();
        for (_rid, cols) in self.table("Refs")? {
            if let (Some(Value::Int(a)), Some(Value::Int(b))) = (cols.first(), cols.get(1)) {
                edges.entry(*a).or_default().push(*b);
            }
        }
        // Seed the DFS from every root (each resolved before any walking, so a
        // missing root fails loudly — like `guix gc --requisites` on an invalid
        // path — rather than after a partial closure).
        let mut stack: Vec<i64> = Vec::with_capacity(roots.len());
        for root in roots {
            let start = *id_of
                .get(root)
                .ok_or_else(|| format!("root `{root}' is not in the store DB"))?;
            stack.push(start);
        }
        // Iterative DFS over the reference graph; the rowid set dedups (handles
        // self-references, cycles, and overlapping roots).
        let mut seen: HashSet<i64> = HashSet::new();
        while let Some(n) = stack.pop() {
            if !seen.insert(n) {
                continue;
            }
            if let Some(neighbors) = edges.get(&n) {
                for &m in neighbors {
                    if !seen.contains(&m) {
                        stack.push(m);
                    }
                }
            }
        }
        // Resolve every reached rowid to its path; a reachable Refs id with no
        // ValidPaths row is a corrupt store DB — error (symmetric with the
        // `store-query references` resolver), don't silently drop it.
        let mut out: Vec<String> = Vec::with_capacity(seen.len());
        for id in &seen {
            out.push(
                path_of
                    .get(id)
                    .cloned()
                    .ok_or_else(|| format!("reachable Refs id {id} has no ValidPaths row"))?,
            );
        }
        out.sort();
        Ok(out)
    }

    /// The Refs graph as a PATH-keyed map (path → referenced paths), resolving
    /// rowids to paths via `ValidPaths`. Every valid path is a node (even with no
    /// out-edges). Unlike `closure` (rowid-keyed, single-db), a path-keyed graph
    /// can be MERGED across dbs — a td.db (a td-built dep + its DIRECT refs) with
    /// guix's db (those refs' TRANSITIVE seeds) — before the walk; that merge is
    /// how a downstream build's closure spans td's own output and guix's seeds.
    /// A Refs id with no `ValidPaths` row is a corrupt db (error, as in `closure`).
    pub fn refs_by_path(&self) -> Result<HashMap<String, Vec<String>>, String> {
        let mut path_of: HashMap<i64, String> = HashMap::new();
        for (rowid, cols) in self.table("ValidPaths")? {
            if let Some(Value::Text(p)) = cols.get(1) {
                path_of.insert(rowid, p.clone());
            }
        }
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for p in path_of.values() {
            out.entry(p.clone()).or_default();
        }
        for (_rid, cols) in self.table("Refs")? {
            if let (Some(Value::Int(a)), Some(Value::Int(b))) = (cols.first(), cols.get(1)) {
                let from = path_of
                    .get(a)
                    .ok_or_else(|| format!("Refs referrer id {a} has no ValidPaths row"))?;
                let to = path_of
                    .get(b)
                    .ok_or_else(|| format!("Refs reference id {b} has no ValidPaths row"))?;
                out.entry(from.clone()).or_default().push(to.clone());
            }
        }
        Ok(out)
    }

    /// Find a table's rootpage by scanning the `sqlite_master` b-tree (rooted at
    /// page 1). Its rows are `(type, name, tbl_name, rootpage, sql)`.
    fn rootpage_of(&self, name: &str) -> Result<usize, String> {
        let mut master = Vec::new();
        self.walk_table(1, &mut master)?;
        for (_rowid, cols) in &master {
            // cols[1] = name, cols[3] = rootpage.
            if let (Some(Value::Text(n)), Some(Value::Int(rp))) = (cols.get(1), cols.get(3)) {
                if n == name {
                    return Ok(*rp as usize);
                }
            }
        }
        Err(format!("table `{name}' not found in sqlite_master"))
    }

    /// The byte slice of page `n` (1-indexed).
    fn page(&self, n: usize) -> Result<&[u8], String> {
        let start = (n - 1) * self.page_size;
        let end = start + self.page_size;
        self.data
            .get(start..end)
            .ok_or_else(|| format!("page {n} out of range"))
    }

    /// Walk a table b-tree rooted at `rootpage`, appending `(rowid, values)` for
    /// every leaf cell in ascending order. Descends interior pages.
    fn walk_table(&self, rootpage: usize, out: &mut Vec<(i64, Vec<Value>)>) -> Result<(), String> {
        let page = self.page(rootpage)?;
        // Page 1 carries the 100-byte file header before its b-tree header.
        let hdr = if rootpage == 1 { HEADER_LEN } else { 0 };
        let page_type = page[hdr];
        let num_cells = u16::from_be_bytes([page[hdr + 3], page[hdr + 4]]) as usize;
        match page_type {
            LEAF => {
                let ptr_array = hdr + 8; // leaf header is 8 bytes
                for i in 0..num_cells {
                    let off = u16::from_be_bytes([
                        page[ptr_array + 2 * i],
                        page[ptr_array + 2 * i + 1],
                    ]) as usize;
                    let (payload_len, n1) = read_varint(page, off)?;
                    let (rowid, n2) = read_varint(page, off + n1)?;
                    let body_start = off + n1 + n2;
                    let body_end = body_start + payload_len as usize;
                    let payload = page
                        .get(body_start..body_end)
                        .ok_or_else(|| "leaf cell payload overruns page (overflow unsupported)".to_string())?;
                    out.push((rowid as i64, parse_record(payload)?));
                }
                Ok(())
            }
            INTERIOR => {
                // Interior header is 12 bytes; bytes 8..12 are the right-most child.
                let ptr_array = hdr + 12;
                for i in 0..num_cells {
                    let off = u16::from_be_bytes([
                        page[ptr_array + 2 * i],
                        page[ptr_array + 2 * i + 1],
                    ]) as usize;
                    // Cell = left-child page number (u32 BE) + rowid key varint.
                    let child = u32::from_be_bytes([
                        page[off], page[off + 1], page[off + 2], page[off + 3],
                    ]) as usize;
                    self.walk_table(child, out)?;
                }
                let right = u32::from_be_bytes([
                    page[hdr + 8], page[hdr + 9], page[hdr + 10], page[hdr + 11],
                ]) as usize;
                self.walk_table(right, out)
            }
            other => Err(format!("unexpected b-tree page type 0x{other:02x} (index/overflow unsupported)")),
        }
    }
}

/// Read a SQLite varint at `off`; return `(value, bytes_consumed)`. The first 8
/// bytes contribute 7 bits each (high bit = continue); a 9th byte contributes a
/// full 8 bits. Inverse of `store_db::put_varint`. Returns `Err` on a varint that
/// runs past the end of `d` (a truncated/corrupt record) rather than panicking.
fn read_varint(d: &[u8], off: usize) -> Result<(u64, usize), String> {
    let mut result: u64 = 0;
    let mut i = 0;
    while i < 9 {
        let byte = *d
            .get(off + i)
            .ok_or_else(|| "varint runs past end of data".to_string())?;
        if i == 8 {
            result = (result << 8) | byte as u64;
            return Ok((result, 9));
        }
        result = (result << 7) | (byte & 0x7f) as u64;
        i += 1;
        if byte & 0x80 == 0 {
            return Ok((result, i));
        }
    }
    Ok((result, 9))
}

/// Decode a SQLite record (a cell payload) into its column values: a header of
/// (header-length, serial-types…) varints, then the value bodies. Inverse of
/// `store_db::record`.
fn parse_record(payload: &[u8]) -> Result<Vec<Value>, String> {
    let (hdr_len, n) = read_varint(payload, 0)?;
    let hdr_end = hdr_len as usize;
    if hdr_end > payload.len() {
        return Err("record header overruns payload".to_string());
    }
    let mut serials = Vec::new();
    let mut p = n;
    while p < hdr_end {
        let (st, used) = read_varint(payload, p)?;
        serials.push(st);
        p += used;
    }
    let mut body = hdr_end;
    let mut values = Vec::with_capacity(serials.len());
    for st in serials {
        let (val, len) = read_value(st, &payload[body..])?;
        body += len;
        values.push(val);
    }
    Ok(values)
}

/// Decode one value of serial type `st` from the front of `b`; return
/// `(value, bytes_consumed)`. Inverse of `store_db::serial`. Returns `Err` rather
/// than panicking if `b` is shorter than the serial type requires.
fn read_value(st: u64, b: &[u8]) -> Result<(Value, usize), String> {
    // The first `n` bytes of `b`, or Err if the body is truncated.
    let take = |n: usize| -> Result<&[u8], String> {
        b.get(0..n)
            .ok_or_else(|| format!("value body of {n} bytes runs past end of record"))
    };
    Ok(match st {
        0 => (Value::Null, 0),
        1 => (Value::Int(be_signed(take(1)?)), 1),
        2 => (Value::Int(be_signed(take(2)?)), 2),
        3 => (Value::Int(be_signed(take(3)?)), 3),
        4 => (Value::Int(be_signed(take(4)?)), 4),
        5 => (Value::Int(be_signed(take(6)?)), 6),
        6 => (Value::Int(be_signed(take(8)?)), 8),
        7 => return Err("serial type 7 (real) unexpected in a store DB".to_string()),
        8 => (Value::Int(0), 0),
        9 => (Value::Int(1), 0),
        10 | 11 => return Err(format!("reserved serial type {st}")),
        n if n % 2 == 0 => {
            let len = (n as usize - 12) / 2;
            (Value::Blob(take(len)?.to_vec()), len)
        }
        n => {
            let len = (n as usize - 13) / 2;
            let s = std::str::from_utf8(take(len)?)
                .map_err(|e| format!("invalid UTF-8 in text column: {e}"))?
                .to_string();
            (Value::Text(s), len)
        }
    })
}

/// A big-endian signed integer of `b.len()` bytes (1..=8), sign-extended.
fn be_signed(b: &[u8]) -> i64 {
    let mut v: i64 = if !b.is_empty() && b[0] & 0x80 != 0 { -1 } else { 0 };
    for &byte in b {
        v = (v << 8) | byte as i64;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_db::{self, Table};

    // The writer's Value and the reader's Value are distinct types; compare by
    // projecting both onto a common shape for the round-trip assertions below.
    fn w(v: &store_db::Value) -> Value {
        match v {
            store_db::Value::Null => Value::Null,
            store_db::Value::Int(i) => Value::Int(*i),
            store_db::Value::Text(s) => Value::Text(s.clone()),
        }
    }

    #[test]
    fn varint_roundtrip() {
        for n in [0u64, 1, 127, 128, 282616, 1_887_497, 41_145_793, (1u64 << 56) - 1, u64::MAX] {
            let mut enc = Vec::new();
            store_db::put_varint_for_test(&mut enc, n);
            let (got, used) = read_varint(&enc, 0).unwrap();
            assert_eq!(got, n, "varint roundtrip for {n}");
            assert_eq!(used, enc.len(), "consumed all bytes for {n}");
        }
    }

    #[test]
    fn reads_back_a_validpaths_like_table() {
        // narSize values that exercise serial types 3 (3-byte) and 4 (4-byte).
        let rows = vec![
            (
                1i64,
                vec![
                    store_db::Value::Null,
                    store_db::Value::Text("/gnu/store/aaa-hello".to_string()),
                    store_db::Value::Text("sha256:0f28ab".to_string()),
                    store_db::Value::Int(1),
                    store_db::Value::Text("/gnu/store/ddd-hello.drv".to_string()),
                    store_db::Value::Int(282616),
                ],
            ),
            (
                3i64,
                vec![
                    store_db::Value::Null,
                    store_db::Value::Text("/gnu/store/bbb-glibc".to_string()),
                    store_db::Value::Text("sha256:deadbe".to_string()),
                    store_db::Value::Int(1),
                    store_db::Value::Null,
                    store_db::Value::Int(41_145_793),
                ],
            ),
        ];
        // Project the writer rows onto reader Values BEFORE moving them into the
        // table (store_db::Value is not Clone — keep the test minimal, don't derive it).
        let expected: Vec<(i64, Vec<Value>)> = rows
            .iter()
            .map(|(rid, vals)| (*rid, vals.iter().map(w).collect()))
            .collect();
        let t = Table {
            name: "ValidPaths",
            sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
            rows,
        };
        let bytes = store_db::write_db(&[t]);
        let db = Db::open(bytes).unwrap();
        let got = db.table("ValidPaths").unwrap();
        assert_eq!(got, expected, "all rows round-trip (rowid + columns)");
    }

    #[test]
    fn reads_all_three_store_tables() {
        let valid = Table {
            name: "ValidPaths",
            sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
            rows: vec![(1, vec![
                store_db::Value::Null,
                store_db::Value::Text("/gnu/store/x".to_string()),
                store_db::Value::Text("sha256:00".to_string()),
                store_db::Value::Int(1),
                store_db::Value::Text("/gnu/store/x.drv".to_string()),
                store_db::Value::Int(7),
            ])],
        };
        let refs = Table {
            name: "Refs",
            sql: "CREATE TABLE Refs (referrer integer, reference integer)",
            rows: vec![(1, vec![store_db::Value::Int(1), store_db::Value::Int(1)])],
        };
        let dout = Table {
            name: "DerivationOutputs",
            sql: "CREATE TABLE DerivationOutputs (drv integer, id text, path text)",
            rows: vec![(1, vec![
                store_db::Value::Int(2),
                store_db::Value::Text("out".to_string()),
                store_db::Value::Text("/gnu/store/x".to_string()),
            ])],
        };
        let db = Db::open(store_db::write_db(&[valid, refs, dout])).unwrap();
        // ValidPaths: the self-reference resolves via the rowid.
        let vp = db.table("ValidPaths").unwrap();
        assert_eq!(vp[0].0, 1);
        assert_eq!(vp[0].1[1], Value::Text("/gnu/store/x".to_string()));
        let r = db.table("Refs").unwrap();
        assert_eq!(r[0].1, vec![Value::Int(1), Value::Int(1)]);
        let d = db.table("DerivationOutputs").unwrap();
        assert_eq!(d[0].1[1], Value::Text("out".to_string()));
    }

    #[test]
    fn rejects_non_sqlite() {
        assert!(Db::open(b"not a database".to_vec()).is_err());
    }

    #[test]
    fn closure_follows_the_refs_graph() {
        // /a -> /b -> /c, /a self-ref; /d is unreachable from /a.
        let vp = |rid: i64, p: &str| {
            (rid, vec![
                store_db::Value::Null,
                store_db::Value::Text(p.to_string()),
                store_db::Value::Text("sha256:00".to_string()),
                store_db::Value::Int(1),
                store_db::Value::Null,
                store_db::Value::Int(1),
            ])
        };
        let valid = Table {
            name: "ValidPaths",
            sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
            rows: vec![vp(1, "/a"), vp(2, "/b"), vp(3, "/c"), vp(4, "/d")],
        };
        let edge = |rid: i64, a: i64, b: i64| {
            (rid, vec![store_db::Value::Int(a), store_db::Value::Int(b)])
        };
        let refs = Table {
            name: "Refs",
            sql: "CREATE TABLE Refs (referrer integer, reference integer)",
            rows: vec![edge(1, 1, 1), edge(2, 1, 2), edge(3, 2, 3)],
        };
        let db = Db::open(store_db::write_db(&[valid, refs])).unwrap();
        assert_eq!(db.closure("/a").unwrap(), vec!["/a", "/b", "/c"]);
        assert_eq!(db.closure("/d").unwrap(), vec!["/d"]); // no out-edges
        assert!(db.closure("/missing").is_err());
    }

    #[test]
    fn closure_roots_unions_and_dedups() {
        // /a -> /b -> /c ; /d -> /b ; /e isolated. /a and /d overlap on /b,/c.
        let vp = |rid: i64, p: &str| {
            (rid, vec![
                store_db::Value::Null,
                store_db::Value::Text(p.to_string()),
                store_db::Value::Text("sha256:00".to_string()),
                store_db::Value::Int(1),
                store_db::Value::Null,
                store_db::Value::Int(1),
            ])
        };
        let valid = Table {
            name: "ValidPaths",
            sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
            rows: vec![vp(1, "/a"), vp(2, "/b"), vp(3, "/c"), vp(4, "/d"), vp(5, "/e")],
        };
        let edge = |rid: i64, a: i64, b: i64| {
            (rid, vec![store_db::Value::Int(a), store_db::Value::Int(b)])
        };
        let refs = Table {
            name: "Refs",
            sql: "CREATE TABLE Refs (referrer integer, reference integer)",
            rows: vec![edge(1, 1, 2), edge(2, 2, 3), edge(3, 4, 2)],
        };
        let db = Db::open(store_db::write_db(&[valid, refs])).unwrap();
        // Union of two roots whose closures overlap on /b,/c — deduped, sorted
        // (== guix gc --requisites /a /d).
        assert_eq!(
            db.closure_roots(&["/a".to_string(), "/d".to_string()]).unwrap(),
            vec!["/a", "/b", "/c", "/d"]
        );
        // Single root in a slice matches closure() exactly.
        assert_eq!(db.closure_roots(&["/a".to_string()]).unwrap(), db.closure("/a").unwrap());
        // Overlapping/duplicate roots fold into one closure.
        assert_eq!(
            db.closure_roots(&["/b".to_string(), "/b".to_string(), "/c".to_string()]).unwrap(),
            vec!["/b", "/c"]
        );
        // An isolated root contributes only itself.
        assert_eq!(db.closure_roots(&["/e".to_string()]).unwrap(), vec!["/e"]);
        // No roots => empty closure.
        assert_eq!(db.closure_roots(&[]).unwrap(), Vec::<String>::new());
        // A missing root among valid ones fails loudly (no partial closure).
        assert!(db.closure_roots(&["/a".to_string(), "/missing".to_string()]).is_err());
    }

    #[test]
    fn truncated_inputs_error_not_panic() {
        // A continue byte with nothing after it: the varint runs past the end.
        assert!(read_varint(&[0x81], 0).is_err());
        // A record header claiming one column of serial type 6 (an 8-byte int)
        // but with no body bytes: the value body is truncated.
        assert!(parse_record(&[0x02, 0x06]).is_err());
    }
}
