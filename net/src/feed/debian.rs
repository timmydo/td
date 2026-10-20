// debian — `td-feed debian-root resolve LOCK`: re-resolve the Debian build
// root's committed lock (BUILD-ROOT-PLAN.md, increment 2).
//
// The lock's input lines name an image tag, a snapshot.debian.org timestamp,
// the archives to read and the packages wanted. This resolves the tag to its
// one amd64 gzip layer, reads that layer's installed-package database, reads
// each archive's Packages index (verified against its InRelease SHA256 list)
// and writes back the closure of wanted packages the layer does not already
// satisfy. It is pin discovery: nothing fetched here is trusted until the
// reviewed lock commits its sha256, and builds fetch only those pins.
//
// InRelease's signature is not checked: the index hashes it lists only steer
// discovery, and what a build trusts is the reviewed lock diff.
use super::{download_verified, sources_dir, write_atomic};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;

const REGISTRY: &str = "registry-1.docker.io";
const SNAPSHOT: &str = "https://snapshot.debian.org/archive";
const INDEX_PATH: &str = "main/binary-amd64/Packages.xz";
const ARCHITECTURES: &[&str] = &["amd64", "all"];
const IMAGE_MANIFEST_TYPES: &[&str] = &[
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];
/// td reads gzip layers only; any other compression is refused at pin time.
const LAYER_MEDIA_TYPES: &[&str] = &[
    "application/vnd.oci.image.layer.v1.tar+gzip",
    "application/vnd.docker.image.rootfs.diff.tar.gzip",
];
const STATUS_PATH: &str = "var/lib/dpkg/status";

/// The lock's input lines: what to resolve.
#[derive(Debug, PartialEq)]
struct Spec {
    repository: String,
    tag: String,
    snapshot: String,
    archives: Vec<(String, String)>,
    want: Vec<String>,
}

fn is_debian_word(word: &str) -> bool {
    !word.is_empty()
        && word.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'+' | b'-')
        })
}

fn is_snapshot_stamp(stamp: &str) -> bool {
    let bytes = stamp.as_bytes();
    bytes.len() == 16
        && bytes.get(8) == Some(&b'T')
        && bytes.get(15) == Some(&b'Z')
        && bytes
            .iter()
            .enumerate()
            .all(|(i, b)| matches!(i, 8 | 15) || b.is_ascii_digit())
}

/// Parse the input lines; resolved lines (`index`, `manifest`, `layer`,
/// `inrelease`, `deb`) are the resolver's own output and are ignored here.
fn parse_spec(text: &str) -> Result<Spec, String> {
    let (mut image, mut snapshot, mut archives, mut want) = (None, None, Vec::new(), Vec::new());
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        let keyword = words.next().unwrap_or_default();
        let rest: Vec<&str> = words.collect();
        let bad = || {
            format!(
                "debian-root lock line {}: malformed `{keyword}' line",
                n + 1
            )
        };
        match (keyword, rest.as_slice()) {
            ("image", [reference]) => {
                let (repository, tag) = reference
                    .rsplit_once(':')
                    .filter(|(repository, tag)| {
                        crate::http::repository_name_is_valid(repository)
                            && crate::http::tag_is_valid(tag)
                    })
                    .ok_or_else(bad)?;
                image = Some((repository.to_string(), tag.to_string()));
            }
            ("snapshot", [stamp]) if is_snapshot_stamp(stamp) => snapshot = Some(stamp.to_string()),
            ("archive", [archive, suite]) if is_debian_word(archive) && is_debian_word(suite) => {
                archives.push((archive.to_string(), suite.to_string()));
            }
            ("want", names) if !names.is_empty() && names.iter().all(|n| is_debian_word(n)) => {
                want.extend(names.iter().map(|n| n.to_string()));
            }
            ("index" | "manifest" | "layer" | "inrelease" | "deb", _) => {}
            _ => return Err(bad()),
        }
    }
    let (repository, tag) = image.ok_or("debian-root lock names no `image'")?;
    if archives.is_empty() || want.is_empty() {
        return Err("debian-root lock needs at least one `archive' and one `want'".into());
    }
    Ok(Spec {
        repository,
        tag,
        snapshot: snapshot.ok_or("debian-root lock names no `snapshot'")?,
        archives,
        want,
    })
}

/// One `Name: value` stanza of a Packages or status file. Continuation lines
/// (multi-line descriptions) are skipped: no field read here spans lines.
fn stanzas(text: &str) -> Vec<BTreeMap<&str, &str>> {
    let mut out = Vec::new();
    let mut current = BTreeMap::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else if !line.starts_with([' ', '\t']) {
            if let Some((name, value)) = line.split_once(':') {
                current.insert(name, value.trim());
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
}

#[derive(Clone, Debug, PartialEq)]
struct Dep {
    name: String,
    constraint: Option<(Op, String)>,
    /// Qualified with an architecture other than amd64 (`:i386`): nothing
    /// in an amd64 root satisfies it.
    foreign: bool,
}

impl Dep {
    fn named(name: &str) -> Dep {
        Dep {
            name: name.to_string(),
            constraint: None,
            foreign: false,
        }
    }
}

/// One alternative of a relation field: `name[:arch] [(op version)]`.
fn parse_dep(text: &str) -> Result<Dep, String> {
    let text = text.trim();
    let (head, constraint) = match text.split_once('(') {
        Some((head, rest)) => {
            let inner = rest
                .strip_suffix(')')
                .ok_or_else(|| format!("unterminated version constraint in `{text}'"))?
                .trim();
            let (op, version) = [
                ("<<", Op::Lt),
                ("<=", Op::Le),
                (">=", Op::Ge),
                (">>", Op::Gt),
                ("=", Op::Eq),
            ]
            .iter()
            .find_map(|(token, op)| inner.strip_prefix(token).map(|v| (*op, v.trim())))
            .ok_or_else(|| format!("unknown version operator in `{text}'"))?;
            (head.trim(), Some((op, version.to_string())))
        }
        None => (text, None),
    };
    let (name, arch) = head.split_once(':').unwrap_or((head, "amd64"));
    if !is_debian_word(name) {
        return Err(format!("malformed package name in `{text}'"));
    }
    Ok(Dep {
        name: name.to_string(),
        constraint,
        foreign: !matches!(arch, "amd64" | "any" | "native"),
    })
}

/// A Depends-style field: comma-separated groups of `|` alternatives.
fn parse_relations(field: Option<&&str>) -> Result<Vec<Vec<Dep>>, String> {
    let Some(field) = field.filter(|f| !f.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    field
        .split(',')
        .map(|group| group.split('|').map(parse_dep).collect())
        .collect()
}

/// dpkg's ordering of one character in a non-digit run: `~` sorts before
/// the end of the string, letters before everything else.
fn char_order(c: Option<u8>) -> i32 {
    match c {
        None => 0,
        Some(b'~') => -1,
        Some(c) if c.is_ascii_digit() => 0,
        Some(c) if c.is_ascii_alphabetic() => i32::from(c),
        Some(c) => i32::from(c) + 256,
    }
}

/// dpkg's `verrevcmp` over an upstream version or a revision.
fn compare_part(a: &[u8], b: &[u8]) -> Ordering {
    let digit = |s: &[u8], i: usize| s.get(i).is_some_and(u8::is_ascii_digit);
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        while (i < a.len() && !digit(a, i)) || (j < b.len() && !digit(b, j)) {
            let (ac, bc) = (char_order(a.get(i).copied()), char_order(b.get(j).copied()));
            if ac != bc {
                return ac.cmp(&bc);
            }
            i += 1;
            j += 1;
        }
        while a.get(i) == Some(&b'0') {
            i += 1;
        }
        while b.get(j) == Some(&b'0') {
            j += 1;
        }
        let mut first_diff = Ordering::Equal;
        while digit(a, i) && digit(b, j) {
            if first_diff == Ordering::Equal {
                first_diff = a.get(i).cmp(&b.get(j));
            }
            i += 1;
            j += 1;
        }
        if digit(a, i) {
            return Ordering::Greater;
        }
        if digit(b, j) {
            return Ordering::Less;
        }
        if first_diff != Ordering::Equal {
            return first_diff;
        }
    }
    Ordering::Equal
}

/// dpkg version ordering: `[epoch:]upstream[-revision]`.
fn compare_versions(a: &str, b: &str) -> Ordering {
    fn split(v: &str) -> (u64, &str, &str) {
        let (epoch, rest) = match v.split_once(':') {
            Some((e, rest)) if e.bytes().all(|b| b.is_ascii_digit()) => {
                (e.parse().unwrap_or(0), rest)
            }
            _ => (0, v),
        };
        let (upstream, revision) = rest.rsplit_once('-').unwrap_or((rest, ""));
        (epoch, upstream, revision)
    }
    let (ae, au, ar) = split(a);
    let (be, bu, br) = split(b);
    ae.cmp(&be)
        .then_with(|| compare_part(au.as_bytes(), bu.as_bytes()))
        .then_with(|| compare_part(ar.as_bytes(), br.as_bytes()))
}

fn satisfies(version: &str, constraint: &Option<(Op, String)>) -> bool {
    let Some((op, want)) = constraint else {
        return true;
    };
    let order = compare_versions(version, want);
    match op {
        Op::Lt => order == Ordering::Less,
        Op::Le => order != Ordering::Greater,
        Op::Eq => order == Ordering::Equal,
        Op::Ge => order != Ordering::Less,
        Op::Gt => order == Ordering::Greater,
    }
}

/// A snapshot candidate, or (with an empty `archive`) an installed package.
#[derive(Clone, Debug, PartialEq)]
struct Package {
    name: String,
    version: String,
    archive: String,
    depends: Vec<Vec<Dep>>,
    /// Breaks and Conflicts: what may not be unpacked beside it.
    conflicts: Vec<Dep>,
    /// Virtual names, each with the version an `(= v)` Provides gives it.
    provides: Vec<(String, Option<String>)>,
    filename: String,
    size: u64,
    sha256: String,
}

impl Package {
    fn from_stanza(stanza: &BTreeMap<&str, &str>, archive: &str) -> Result<Package, String> {
        let field = |name: &str| {
            stanza
                .get(name)
                .map(|v| v.to_string())
                .ok_or_else(|| format!("{archive} stanza lacks {name}"))
        };
        let optional = |name: &str| stanza.get(name).map_or_else(String::new, |v| v.to_string());
        let mut depends = parse_relations(stanza.get("Pre-Depends"))?;
        depends.extend(parse_relations(stanza.get("Depends"))?);
        let provides = parse_relations(stanza.get("Provides"))?
            .into_iter()
            .flatten()
            .map(|dep| match dep.constraint {
                None => Ok((dep.name, None)),
                Some((Op::Eq, version)) => Ok((dep.name, Some(version))),
                Some(_) => Err(format!("{archive}: Provides {} is not `='", dep.name)),
            })
            .collect::<Result<_, String>>()?;
        Ok(Package {
            name: field("Package")?,
            version: field("Version")?,
            archive: archive.to_string(),
            depends,
            conflicts: parse_relations(stanza.get("Breaks"))?
                .into_iter()
                .chain(parse_relations(stanza.get("Conflicts"))?)
                .flatten()
                .collect(),
            provides,
            filename: optional("Filename"),
            size: stanza
                .get("Size")
                .map_or(Ok(0), |s| s.parse())
                .map_err(|_| format!("{archive}: malformed Size"))?,
            sha256: optional("SHA256"),
        })
    }

    /// Whether this package's Provides satisfies `dep`. A versioned dep is
    /// met only by a versioned Provides.
    fn provides_for(&self, dep: &Dep) -> bool {
        self.provides.iter().any(|(name, version)| {
            *name == dep.name
                && match &dep.constraint {
                    None => true,
                    Some(_) => version
                        .as_deref()
                        .is_some_and(|v| satisfies(v, &dep.constraint)),
                }
        })
    }

    /// Whether this package, by its own name or a Provides, satisfies `dep`.
    fn meets(&self, dep: &Dep) -> bool {
        !dep.foreign
            && ((self.name == dep.name && satisfies(&self.version, &dep.constraint))
                || self.provides_for(dep))
    }
}

/// Every candidate the archives offer: the newest version per name.
#[derive(Default)]
struct Universe {
    packages: BTreeMap<String, Package>,
}

impl Universe {
    fn add_index(&mut self, archive: &str, text: &str) -> Result<(), String> {
        for stanza in stanzas(text) {
            let architecture = stanza.get("Architecture").copied().unwrap_or_default();
            if !ARCHITECTURES.contains(&architecture) {
                continue;
            }
            let package = Package::from_stanza(&stanza, archive)?;
            if package.filename.is_empty() || package.sha256.is_empty() {
                return Err(format!(
                    "{archive}: {} lacks Filename or SHA256",
                    package.name
                ));
            }
            let newer = self.packages.get(&package.name).is_none_or(|old| {
                compare_versions(&package.version, &old.version) == Ordering::Greater
            });
            if newer {
                self.packages.insert(package.name.clone(), package);
            }
        }
        Ok(())
    }

    /// The package a dependency resolves to: the real package of that name
    /// when it satisfies the constraint, else the first (by name) package
    /// whose Provides does.
    fn choose(&self, dep: &Dep) -> Result<&Package, String> {
        if dep.foreign {
            return Err(format!("{} names a foreign architecture", dep.name));
        }
        if let Some(package) = self.packages.get(&dep.name).filter(|p| p.meets(dep)) {
            return Ok(package);
        }
        if let Some(provider) = self.packages.values().find(|p| p.provides_for(dep)) {
            return Ok(provider);
        }
        Err(match self.packages.get(&dep.name) {
            Some(package) => format!(
                "the snapshot has {} {}, which does not satisfy {}",
                dep.name,
                package.version,
                describe(dep)
            ),
            None => format!("nothing in the snapshot provides {}", describe(dep)),
        })
    }
}

fn describe(dep: &Dep) -> String {
    match &dep.constraint {
        None => dep.name.clone(),
        Some((op, version)) => format!("{} ({op:?} {version})", dep.name),
    }
}

/// What the image's dpkg database says is installed (or held).
fn parse_status(text: &str) -> Result<BTreeMap<String, Package>, String> {
    let mut installed = BTreeMap::new();
    for stanza in stanzas(text) {
        let status = stanza.get("Status").copied().unwrap_or_default();
        if !matches!(status, "install ok installed" | "hold ok installed") {
            continue;
        }
        let package = Package::from_stanza(&stanza, "")?;
        installed.insert(package.name.clone(), package);
    }
    Ok(installed)
}

/// The root after unpacking: each selected package replaces the installed
/// one of its name.
struct Root<'a> {
    installed: &'a BTreeMap<String, Package>,
    selected: &'a BTreeMap<String, Package>,
}

impl Root<'_> {
    fn get(&self, name: &str) -> Option<&Package> {
        self.selected.get(name).or_else(|| self.installed.get(name))
    }

    fn packages(&self) -> impl Iterator<Item = &Package> {
        self.installed
            .values()
            .filter(|p| !self.selected.contains_key(&p.name))
            .chain(self.selected.values())
    }

    fn satisfied(&self, dep: &Dep) -> bool {
        self.get(&dep.name).is_some_and(|p| p.meets(dep))
            || self.packages().any(|p| p.provides_for(dep))
    }

    /// Each Depends, Breaks or Conflicts the root violates: the packages
    /// whose upgrade could repair it, and why.
    fn violations(&self) -> Vec<(Vec<String>, String)> {
        let mut out = Vec::new();
        for package in self.packages() {
            for group in &package.depends {
                if !group.iter().any(|alt| self.satisfied(alt)) {
                    let wanted: Vec<String> = group.iter().map(describe).collect();
                    out.push((
                        vec![package.name.clone()],
                        format!(
                            "{} {} needs {}",
                            package.name,
                            package.version,
                            wanted.join(" | ")
                        ),
                    ));
                }
            }
            for conflict in &package.conflicts {
                for target in self
                    .packages()
                    .filter(|t| t.name != package.name && t.meets(conflict))
                {
                    out.push((
                        vec![target.name.clone(), package.name.clone()],
                        format!(
                            "{} {} breaks or conflicts with {} {}",
                            package.name, package.version, target.name, target.version
                        ),
                    ));
                }
            }
        }
        out
    }
}

/// The packages to unpack over the image: the closure of `want` over
/// Pre-Depends and Depends, minus what the image already satisfies. A
/// selected package replaces the installed one of its name, never with an
/// older version. When the queue drains, the whole root must satisfy every
/// Depends, Breaks and Conflicts. An installed package that can repair a
/// violation by upgrading is upgraded in turn; any other violation is an
/// error. Each round selects at least one new name, so it terminates.
fn resolve(
    universe: &Universe,
    installed: &BTreeMap<String, Package>,
    want: &[String],
) -> Result<Vec<Package>, String> {
    let mut selected: BTreeMap<String, Package> = BTreeMap::new();
    let mut queue: VecDeque<Dep> = want.iter().map(|name| Dep::named(name)).collect();
    loop {
        while let Some(dep) = queue.pop_front() {
            let root = Root {
                installed,
                selected: &selected,
            };
            if root.satisfied(&dep) {
                continue;
            }
            let package = universe.choose(&dep)?.clone();
            if selected.contains_key(&package.name) {
                return Err(format!(
                    "{} is already selected at a version that does not satisfy {}",
                    package.name,
                    describe(&dep)
                ));
            }
            admit(universe, installed, &mut selected, &mut queue, package)?;
        }
        let root = Root {
            installed,
            selected: &selected,
        };
        let violations = root.violations();
        if violations.is_empty() {
            return Ok(selected.into_values().collect());
        }
        let mut upgrades = Vec::new();
        let mut unfixable = Vec::new();
        for (candidates, why) in violations {
            let upgrade = candidates.iter().find_map(|name| {
                let old = installed
                    .get(name)
                    .filter(|_| !selected.contains_key(name))?;
                universe
                    .packages
                    .get(name)
                    .filter(|new| compare_versions(&new.version, &old.version) == Ordering::Greater)
            });
            match upgrade {
                Some(new) => upgrades.push(new.clone()),
                None => unfixable.push(why),
            }
        }
        if !unfixable.is_empty() {
            return Err(format!(
                "the resolved root is inconsistent: {}",
                unfixable.join("; ")
            ));
        }
        for package in upgrades {
            if !selected.contains_key(&package.name) {
                admit(universe, installed, &mut selected, &mut queue, package)?;
            }
        }
    }
}

/// Select `package`, refusing a downgrade of what the image has, and queue
/// one alternative of each dependency group the root does not yet satisfy.
fn admit(
    universe: &Universe,
    installed: &BTreeMap<String, Package>,
    selected: &mut BTreeMap<String, Package>,
    queue: &mut VecDeque<Dep>,
    package: Package,
) -> Result<(), String> {
    if let Some(old) = installed.get(&package.name) {
        if compare_versions(&package.version, &old.version) == Ordering::Less {
            return Err(format!(
                "the snapshot would downgrade {} {} to {}: move the snapshot later",
                old.name, old.version, package.version
            ));
        }
    }
    let root = Root {
        installed,
        selected,
    };
    for group in &package.depends {
        if group.iter().any(|alt| root.satisfied(alt)) {
            continue;
        }
        let mut reasons = Vec::new();
        let alt = group.iter().find(|alt| match universe.choose(alt) {
            Ok(chosen) if !selected.contains_key(&chosen.name) => true,
            Ok(chosen) => {
                reasons.push(format!("{} is selected at {}", chosen.name, chosen.version));
                false
            }
            Err(reason) => {
                reasons.push(reason);
                false
            }
        });
        let Some(alt) = alt else {
            return Err(format!(
                "{} needs one of: {}",
                package.name,
                reasons.join("; ")
            ));
        };
        queue.push_back(alt.clone());
    }
    selected.insert(package.name.clone(), package);
    Ok(())
}

/// The `SHA256:` entry of an InRelease file for one index path.
fn inrelease_entry(text: &str, path: &str) -> Option<(String, u64)> {
    let mut in_sha256 = false;
    for line in text.lines() {
        if !line.starts_with(' ') {
            in_sha256 = line.trim() == "SHA256:";
            continue;
        }
        if in_sha256 {
            let mut fields = line.split_whitespace();
            if let (Some(hash), Some(size), Some(name)) =
                (fields.next(), fields.next(), fields.next())
            {
                if name == path {
                    return Some((hash.to_string(), size.parse().ok()?));
                }
            }
        }
    }
    None
}

/// A NUL-terminated header field. Names stay bytes: a binary pax value or a
/// name cut mid-character must not hide the entry being looked for.
fn c_bytes(bytes: &[u8]) -> &[u8] {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    bytes.get(..end).unwrap_or_default()
}

/// The `path` and `size` records of a pax extended header; other records'
/// values (binary xattrs among them) are skipped undecoded.
fn pax_records(mut data: &[u8]) -> Option<(Option<Vec<u8>>, Option<usize>)> {
    let (mut path, mut size) = (None, None);
    while !data.is_empty() {
        let space = data.iter().position(|b| *b == b' ')?;
        let length: usize = std::str::from_utf8(data.get(..space)?).ok()?.parse().ok()?;
        let record = data.get(space.checked_add(1)?..length)?;
        let record = record.strip_suffix(b"\n")?;
        let equals = record.iter().position(|b| *b == b'=')?;
        let value = record.get(equals.checked_add(1)?..)?;
        match record.get(..equals)? {
            b"path" => path = Some(value.to_vec()),
            b"size" => size = Some(std::str::from_utf8(value).ok()?.parse().ok()?),
            _ => {}
        }
        data = data.get(length..)?;
    }
    Some((path, size))
}

fn strip_dot_slash(mut name: &[u8]) -> &[u8] {
    while let Some(rest) = name.strip_prefix(b"./") {
        name = rest;
    }
    name
}

/// The contents of regular file `path` in an uncompressed tar stream, as
/// extraction would leave it: the last entry of that name wins. GNU long
/// names and pax `path`/`size` records apply to the entry after them. A
/// malformed or truncated stream yields `None`.
fn tar_member<'a>(tar: &'a [u8], path: &str) -> Option<&'a [u8]> {
    let (mut offset, mut found) = (0usize, None);
    let (mut long_name, mut pax_path, mut pax_size) = (None, None, None);
    loop {
        let header = tar.get(offset..offset.checked_add(512)?)?;
        if header.iter().all(|b| *b == 0) {
            return found;
        }
        let kind = header.get(156).copied().unwrap_or_default();
        let meta = matches!(kind, b'L' | b'K' | b'x' | b'g');
        let octal = std::str::from_utf8(c_bytes(header.get(124..136)?)).ok()?;
        let mut size = usize::try_from(u64::from_str_radix(octal.trim(), 8).ok()?).ok()?;
        let mut name = c_bytes(header.get(..100)?).to_vec();
        if !meta {
            size = pax_size.take().unwrap_or(size);
            let ustar = header.get(257..263) == Some(b"ustar\0".as_slice());
            let prefix = c_bytes(header.get(345..500)?);
            name = match (long_name.take(), pax_path.take()) {
                (_, Some(pax)) => pax,
                (Some(long), None) => long,
                (None, None) if ustar && !prefix.is_empty() => [prefix, b"/", &name].concat(),
                (None, None) => name,
            };
        }
        let data_start = offset.checked_add(512)?;
        let data = tar.get(data_start..data_start.checked_add(size)?)?;
        match kind {
            b'L' => long_name = Some(c_bytes(data).to_vec()),
            b'x' => (pax_path, pax_size) = pax_records(data)?,
            b'K' | b'g' => {}
            _ if strip_dot_slash(&name) == path.as_bytes() => {
                found = matches!(kind, b'0' | 0).then_some(data);
            }
            _ => {}
        }
        offset = data_start.checked_add(size.div_ceil(512).checked_mul(512)?)?;
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn json_str<'a>(value: &'a td_engine::json::Json, key: &str) -> Option<&'a str> {
    value.get(key).and_then(td_engine::json::Json::as_str)
}

fn digest_hex(digest: &str) -> Result<&str, String> {
    digest
        .strip_prefix("sha256:")
        .filter(|hex| {
            hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
        .ok_or_else(|| format!("`{digest}' is not a sha256 digest"))
}

/// The amd64 manifest an index names, or the manifest itself when the tag
/// already names a single-platform image.
fn amd64_manifest_digest(index: &td_engine::json::Json) -> Result<Option<String>, String> {
    let Some(manifests) = index
        .get("manifests")
        .and_then(td_engine::json::Json::as_arr)
    else {
        return Ok(None);
    };
    let matches: Vec<&str> = manifests
        .iter()
        .filter(|entry| {
            let platform = entry.get("platform");
            let field = |key| platform.and_then(|p| json_str(p, key));
            field("os") == Some("linux")
                && field("architecture") == Some("amd64")
                && platform.and_then(|p| p.get("variant")).is_none()
        })
        .filter_map(|entry| json_str(entry, "digest"))
        .collect();
    match matches.as_slice() {
        [digest] => Ok(Some(digest.to_string())),
        _ => Err(format!(
            "the image index names {} linux/amd64 manifests, not one",
            matches.len()
        )),
    }
}

/// The single gzip layer of an image manifest: its digest and size.
fn single_gzip_layer(manifest: &td_engine::json::Json) -> Result<(String, u64), String> {
    let media_type = json_str(manifest, "mediaType").unwrap_or_default();
    if !IMAGE_MANIFEST_TYPES.contains(&media_type) {
        return Err(format!(
            "unsupported image manifest media type `{media_type}'"
        ));
    }
    let layers = manifest
        .get("layers")
        .and_then(td_engine::json::Json::as_arr)
        .ok_or("image manifest lists no layers")?;
    let [layer] = layers else {
        return Err(format!(
            "the image has {} layers; the build root takes exactly one",
            layers.len()
        ));
    };
    let layer_type = json_str(layer, "mediaType").unwrap_or_default();
    if !LAYER_MEDIA_TYPES.contains(&layer_type) {
        return Err(format!(
            "unsupported layer media type `{layer_type}': td reads gzip layers only"
        ));
    }
    let digest = json_str(layer, "digest").ok_or("layer has no digest")?;
    digest_hex(digest)?;
    let size = match layer.get("size") {
        Some(td_engine::json::Json::Num(n)) => n.parse().map_err(|_| "malformed layer size")?,
        _ => return Err("layer has no size".into()),
    };
    Ok((digest.to_string(), size))
}

/// Everything the resolver writes back below the input lines.
struct Resolved {
    index: String,
    manifest: String,
    layer_hex: String,
    layer_size: u64,
    inreleases: Vec<(String, String, String)>,
    packages: Vec<Package>,
}

fn layer_file(spec: &Spec) -> String {
    format!("debian-{}-amd64-layer.tar.gz", spec.tag)
}

/// The pool basename, which becomes a flat `~/.td/sources` file name.
fn deb_file(package: &Package) -> Result<&str, String> {
    package
        .filename
        .rsplit('/')
        .next()
        .filter(|file| {
            file.ends_with(".deb")
                && file
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_alphanumeric())
                && file.bytes().all(|b| {
                    b.is_ascii_alphanumeric()
                        || matches!(b, b'.' | b'+' | b'-' | b'_' | b'~' | b'%')
                })
        })
        .ok_or_else(|| {
            format!(
                "{}: unusable pool filename {}",
                package.name, package.filename
            )
        })
}

fn render(spec_text: &str, spec: &Spec, resolved: &Resolved) -> Result<String, String> {
    let mut out = String::new();
    for line in spec_text.lines() {
        let keyword = line.split_whitespace().next().unwrap_or_default();
        if !matches!(
            keyword,
            "index" | "manifest" | "layer" | "inrelease" | "deb"
        ) && !line.starts_with("# resolved")
        {
            out.push_str(line);
            out.push('\n');
        }
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out.push_str("\n# resolved by `td-feed debian-root resolve`; review the diff\n");
    out.push_str(&format!(
        "index {}\nmanifest {}\n",
        resolved.index, resolved.manifest
    ));
    out.push_str(&format!(
        "layer debian-root-layer {} {} {} https://{REGISTRY}/v2/{}/blobs/sha256:{}\n",
        layer_file(spec),
        resolved.layer_hex,
        resolved.layer_size,
        spec.repository,
        resolved.layer_hex
    ));
    for (archive, suite, hex) in &resolved.inreleases {
        out.push_str(&format!("inrelease {archive} {suite} {hex}\n"));
    }
    for package in &resolved.packages {
        out.push_str(&format!(
            "deb debian-root-{} {} {} {} {SNAPSHOT}/{}/{}/{} {}\n",
            package.name,
            deb_file(package)?,
            package.sha256,
            package.size,
            package.archive,
            spec.snapshot,
            package.filename,
            package.version
        ));
    }
    Ok(out)
}

fn fetch_verified(url: &str, want_hex: &str, want_size: u64) -> Result<Vec<u8>, String> {
    let body = crate::http::get_body(url)?;
    if u64::try_from(body.len()).ok() != Some(want_size) || sha256_hex(&body) != want_hex {
        return Err(format!("{url} does not match its InRelease entry"));
    }
    Ok(body)
}

fn resolve_lock(path: &Path) -> Result<(), String> {
    let spec_text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let spec = parse_spec(&spec_text)?;

    let manifests = format!("https://{REGISTRY}/v2/{}/manifests", spec.repository);
    let index_body = crate::http::get_registry_manifest(&format!("{manifests}/{}", spec.tag))?;
    let index_text = std::str::from_utf8(&index_body).map_err(|_| "index is not UTF-8")?;
    let index_json = td_engine::json::parse(index_text)?;
    let index = format!("sha256:{}", sha256_hex(&index_body));
    let recorded = spec_text
        .lines()
        .find_map(|line| line.strip_prefix("index "))
        .map(str::trim);
    if recorded.is_some_and(|old| old != index) {
        eprintln!(
            ">> debian-root: WARNING: {}:{} now names index {index}, not the recorded one; \
             if the image line is unchanged the tag was re-pushed, so review the layer diff",
            spec.repository, spec.tag
        );
    }
    let (manifest, manifest_json) = match amd64_manifest_digest(&index_json)? {
        Some(digest) => {
            let body = crate::http::get_registry_manifest(&format!("{manifests}/{digest}"))?;
            let text = std::str::from_utf8(&body).map_err(|_| "manifest is not UTF-8")?;
            (digest, td_engine::json::parse(text)?)
        }
        None => (index.clone(), index_json),
    };
    let (layer_digest, layer_size) = single_gzip_layer(&manifest_json)?;
    let layer_hex = digest_hex(&layer_digest)?.to_string();
    let layer_path = sources_dir().join(layer_file(&spec));
    download_verified(
        &format!(
            "https://{REGISTRY}/v2/{}/blobs/{layer_digest}",
            spec.repository
        ),
        &layer_path,
        &layer_hex,
    )?;
    let tar = td_engine::gzip::decompress_file(&layer_path)?;
    let status = tar_member(&tar, STATUS_PATH)
        .ok_or_else(|| format!("the image layer has no {STATUS_PATH}"))?;
    let installed = parse_status(std::str::from_utf8(status).map_err(|_| "status is not UTF-8")?)?;
    eprintln!(
        ">> debian-root: {} {} -> layer {layer_digest}, {} packages installed",
        spec.repository,
        spec.tag,
        installed.len()
    );

    let mut universe = Universe::default();
    let mut inreleases = Vec::new();
    for (archive, suite) in &spec.archives {
        let dists = format!("{SNAPSHOT}/{archive}/{}/dists/{suite}", spec.snapshot);
        let inrelease = crate::http::get_body(&format!("{dists}/InRelease"))?;
        let inrelease_text = std::str::from_utf8(&inrelease)
            .map_err(|_| format!("{dists}/InRelease is not UTF-8"))?;
        let (hex, size) = inrelease_entry(inrelease_text, INDEX_PATH)
            .ok_or_else(|| format!("{dists}/InRelease lists no {INDEX_PATH}"))?;
        let compressed = fetch_verified(&format!("{dists}/{INDEX_PATH}"), &hex, size)?;
        let mut packages = Vec::new();
        td_engine::xz::decompress_reader_to(&mut compressed.as_slice(), &mut packages)?;
        let text =
            std::str::from_utf8(&packages).map_err(|_| format!("{dists} index is not UTF-8"))?;
        universe.add_index(archive, text)?;
        inreleases.push((archive.clone(), suite.clone(), sha256_hex(&inrelease)));
    }

    let packages = resolve(&universe, &installed, &spec.want)?;
    let total: u64 = packages.iter().map(|p| p.size).sum();
    let resolved = Resolved {
        index,
        manifest,
        layer_hex,
        layer_size,
        inreleases,
        packages,
    };
    write_atomic(path, render(&spec_text, &spec, &resolved)?.as_bytes())?;
    eprintln!(
        ">> debian-root: {} packages ({total} bytes) written to {}",
        resolved.packages.len(),
        path.display()
    );
    Ok(())
}

pub(super) fn run(args: &[String]) -> Result<(), String> {
    match args {
        [verb, lock] if verb == "resolve" => resolve_lock(Path::new(lock)),
        _ => Err("usage: td-feed debian-root resolve LOCK".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(name: &str, version: &str, depends: &str, provides: &str) -> String {
        format!(
            "Package: {name}\nVersion: {version}\nArchitecture: amd64\n\
             Depends: {depends}\nProvides: {provides}\n\
             Filename: pool/main/x/{name}/{name}_{version}_amd64.deb\nSize: 10\n\
             SHA256: {}\nDescription: one line\n continued\n\n",
            "0".repeat(64)
        )
    }

    fn names(packages: &[Package]) -> Vec<&str> {
        packages.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn dpkg_version_order() {
        for (a, b) in [
            ("1.0", "1.1"),
            ("1.0~rc1", "1.0"),
            ("1.0", "1.0-1"),
            ("1.0-1", "1.0-2"),
            ("2.0", "1:0.1"),
            ("2.41-12", "2.41-12+deb13u1"),
            ("1.0a", "1.0b"),
            ("1.9", "1.10"),
            ("1.0-1~bpo1", "1.0-1"),
            ("1.0+b1", "1.0.1"),
        ] {
            assert_eq!(compare_versions(a, b), Ordering::Less, "{a} < {b}");
            assert_eq!(compare_versions(b, a), Ordering::Greater, "{b} > {a}");
        }
        assert_eq!(compare_versions("1.01", "1.1"), Ordering::Equal);
        assert_eq!(compare_versions("0:1.0", "1.0"), Ordering::Equal);
    }

    #[test]
    fn relations_parse_alternatives_constraints_and_qualifiers() {
        let field = "libc6 (>= 2.38), perl:any, awk | mawk (<< 2), python3:amd64 (= 3.13.5-1)";
        let groups = parse_relations(Some(&field)).unwrap();
        assert_eq!(groups.len(), 4);
        assert_eq!(
            groups[0],
            vec![Dep {
                name: "libc6".into(),
                constraint: Some((Op::Ge, "2.38".into())),
                foreign: false,
            }]
        );
        assert_eq!(groups[1][0].name, "perl");
        assert_eq!(groups[2][1].constraint, Some((Op::Lt, "2".into())));
        assert_eq!(groups[3][0].constraint, Some((Op::Eq, "3.13.5-1".into())));
        assert!(parse_relations(Some(&"libc6 (>= 2.38")).is_err());
        assert!(parse_relations(Some(&"libc6 (~ 2)")).is_err());
        assert!(parse_relations(Some(&"Bad_Name")).is_err());
    }

    #[test]
    fn the_closure_skips_what_the_image_has_and_follows_the_rest() {
        let mut universe = Universe::default();
        let index = [
            package("gcc", "14.2", "cpp (= 14.2), libc6-dev | libc-dev, awk", ""),
            package("cpp", "14.2", "libc6 (>= 2.41)", ""),
            package("libc6-dev", "2.41-12", "libc6 (= 2.41-12)", "libc-dev"),
            package("libc6", "2.41-12", "", ""),
            package("mawk", "1.3", "", "awk"),
            package("gawk", "5.2", "", "awk"),
        ]
        .concat();
        universe.add_index("debian", &index).unwrap();
        let installed = parse_status(
            "Package: libc6\nStatus: install ok installed\nVersion: 2.41-12\n\n\
             Package: mawk\nStatus: install ok installed\nVersion: 1.3\nProvides: awk\n\n\
             Package: removed\nStatus: deinstall ok config-files\nVersion: 1\n",
        )
        .unwrap();

        let packages = resolve(&universe, &installed, &["gcc".into()]).unwrap();

        assert_eq!(names(&packages), ["cpp", "gcc", "libc6-dev"]);
        assert!(!installed.contains_key("removed"));
    }

    #[test]
    fn an_installed_package_too_old_for_a_constraint_is_upgraded() {
        let mut universe = Universe::default();
        universe
            .add_index(
                "debian",
                &[
                    package("tool", "1", "libc6 (>= 2.41-12+deb13u1)", ""),
                    package("libc6", "2.41-12", "", ""),
                ]
                .concat(),
            )
            .unwrap();
        universe
            .add_index(
                "debian-security",
                &package("libc6", "2.41-12+deb13u1", "", ""),
            )
            .unwrap();
        let installed =
            parse_status("Package: libc6\nStatus: install ok installed\nVersion: 2.41-12\n")
                .unwrap();

        let packages = resolve(&universe, &installed, &["tool".into()]).unwrap();

        assert_eq!(names(&packages), ["libc6", "tool"]);
        assert_eq!(packages[0].archive, "debian-security");
        assert_eq!(packages[0].version, "2.41-12+deb13u1");
    }

    #[test]
    fn an_unsatisfiable_closure_is_an_error() {
        let mut universe = Universe::default();
        universe
            .add_index(
                "debian",
                &[
                    package("tool", "1", "missing | absent", ""),
                    package("old", "1", "", ""),
                ]
                .concat(),
            )
            .unwrap();
        let none = BTreeMap::new();
        let error = resolve(&universe, &none, &["tool".into()]).unwrap_err();
        assert!(error.contains("missing"), "{error}");
        let error = resolve(&universe, &none, &["nothing".into()]).unwrap_err();
        assert!(error.contains("provides nothing"), "{error}");
        universe
            .add_index("debian", &package("needs-new", "1", "old (>= 2)", ""))
            .unwrap();
        let error = resolve(&universe, &none, &["needs-new".into()]).unwrap_err();
        assert!(error.contains("does not satisfy"), "{error}");
    }

    #[test]
    fn other_architectures_are_not_candidates() {
        let mut universe = Universe::default();
        let arm = package("tool", "1", "", "").replace("amd64", "arm64");
        universe.add_index("debian", &arm).unwrap();
        assert!(universe.packages.is_empty());
    }

    #[test]
    fn inrelease_names_the_index_hash() {
        let text = "Origin: Debian\nMD5Sum:\n abc 10 main/binary-amd64/Packages.xz\nSHA256:\n \
                    111 5 main/binary-amd64/Packages\n 222 9 main/binary-amd64/Packages.xz\n\
                    -----BEGIN PGP SIGNATURE-----\n";
        assert_eq!(
            inrelease_entry(text, INDEX_PATH),
            Some(("222".to_string(), 9))
        );
        assert_eq!(inrelease_entry(text, "main/binary-arm64/Packages.xz"), None);
    }

    fn tar_entry(name: impl AsRef<[u8]>, kind: u8, body: &[u8]) -> Vec<u8> {
        let name = name.as_ref();
        let mut header = [0u8; 512];
        header[..name.len()].copy_from_slice(name);
        let size = format!("{:011o}\0", body.len());
        header[124..136].copy_from_slice(size.as_bytes());
        header[156] = kind;
        let mut out = header.to_vec();
        out.extend_from_slice(body);
        out.resize(out.len().div_ceil(512) * 512, 0);
        out
    }

    #[test]
    fn the_status_file_is_found_in_the_layer_tar() {
        let mut tar = tar_entry("./var/lib/dpkg/", b'5', b"");
        tar.extend(tar_entry("./var/lib/dpkg/status-old", b'0', b"old"));
        tar.extend(tar_entry("./var/lib/dpkg/status", b'0', b"Package: x\n"));
        tar.extend([0u8; 1024]);
        assert_eq!(tar_member(&tar, STATUS_PATH), Some(&b"Package: x\n"[..]));
        assert_eq!(tar_member(&tar, "etc/passwd"), None);
        assert_eq!(tar_member(&tar[..600], STATUS_PATH), None);
    }

    fn stanza(name: &str, version: &str, fields: &str) -> String {
        format!(
            "Package: {name}\nVersion: {version}\nArchitecture: amd64\n{fields}\
             Filename: pool/main/x/{name}/{name}_{version}_amd64.deb\nSize: 10\n\
             SHA256: {}\n\n",
            "0".repeat(64)
        )
    }

    fn status(entries: &[(&str, &str, &str)]) -> BTreeMap<String, Package> {
        let text: String = entries
            .iter()
            .map(|(name, version, fields)| {
                format!(
                    "Package: {name}\nStatus: install ok installed\nVersion: {version}\n{fields}\n"
                )
            })
            .collect();
        parse_status(&text).unwrap()
    }

    fn universe(archives: &[(&str, &[String])]) -> Universe {
        let mut universe = Universe::default();
        for (archive, stanzas) in archives {
            universe.add_index(archive, &stanzas.concat()).unwrap();
        }
        universe
    }

    fn want(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn an_upgrade_that_breaks_an_earlier_selection_is_refused() {
        let snapshot = [
            stanza("p", "1", "Depends: libfoo (<< 3)\n"),
            stanza("q", "1", "Depends: libfoo (>= 3)\n"),
            stanza("libfoo", "3", ""),
        ];
        let universe = universe(&[("debian", &snapshot)]);
        let installed = status(&[("libfoo", "2", "")]);

        let error = resolve(&universe, &installed, &want(&["p", "q"])).unwrap_err();

        assert!(error.contains("p 1 needs libfoo (Lt 3)"), "{error}");
    }

    #[test]
    fn a_selected_package_replaces_the_installed_one_for_later_constraints() {
        let snapshot = [
            stanza("q", "1", "Depends: libfoo (>= 3)\n"),
            stanza("r", "1", "Depends: libfoo (= 2)\n"),
            stanza("libfoo", "3", ""),
        ];
        let universe = universe(&[("debian", &snapshot)]);
        let installed = status(&[("libfoo", "2", "")]);

        let error = resolve(&universe, &installed, &want(&["q", "r"])).unwrap_err();

        assert!(error.contains("libfoo"), "{error}");
    }

    #[test]
    fn an_installed_package_an_upgrade_breaks_is_upgraded_in_turn() {
        let snapshot = [
            stanza("tool", "1", "Depends: libc6 (>= 2)\n"),
            stanza("libc6", "2", "Breaks: old-user (<< 2)\n"),
            stanza("libc-bin", "2", "Depends: libc6 (= 2)\n"),
            stanza("old-user", "2", ""),
        ];
        let universe = universe(&[("debian", &snapshot)]);
        let installed = status(&[
            ("libc6", "1", ""),
            ("libc-bin", "1", "Depends: libc6 (= 1)\n"),
            ("old-user", "1", ""),
        ]);

        let packages = resolve(&universe, &installed, &want(&["tool"])).unwrap();

        assert_eq!(names(&packages), ["libc-bin", "libc6", "old-user", "tool"]);
    }

    #[test]
    fn a_broken_installed_package_without_a_newer_version_is_an_error() {
        let snapshot = [
            stanza("tool", "1", "Depends: libc6 (>= 2)\n"),
            stanza("libc6", "2", ""),
        ];
        let universe = universe(&[("debian", &snapshot)]);
        let installed = status(&[
            ("libc6", "1", ""),
            ("pinned", "1", "Depends: libc6 (= 1)\n"),
        ]);

        let error = resolve(&universe, &installed, &want(&["tool"])).unwrap_err();

        assert!(error.contains("pinned 1 needs libc6 (Eq 1)"), "{error}");
    }

    /// Debian's t64 rename: the new name Provides and Breaks the old one. An
    /// upgrade of the old name that the new name's Provides satisfies used to
    /// re-queue forever; it must end in a verdict.
    #[test]
    fn a_provided_upgrade_target_terminates() {
        let snapshot = [
            stanza("tool", "1", "Depends: libfoo-t64\n"),
            stanza(
                "libfoo-t64",
                "2",
                "Provides: libfoo (= 2)\nBreaks: libfoo (<< 2)\n",
            ),
            stanza("libfoo", "1.5", ""),
        ];
        let universe = universe(&[("debian", &snapshot)]);
        let installed = status(&[("libfoo", "1", "")]);

        let error = resolve(&universe, &installed, &want(&["tool"])).unwrap_err();

        assert!(
            error.contains("breaks or conflicts with libfoo 1.5"),
            "{error}"
        );
    }

    #[test]
    fn an_installed_breaker_is_upgraded_when_its_target_is_selected() {
        let snapshot = [
            stanza("tool", "1", "Depends: libfoo (>= 2)\n"),
            stanza("libfoo", "2", ""),
            stanza("old", "2", ""),
        ];
        let universe = universe(&[("debian", &snapshot)]);
        let installed = status(&[("libfoo", "1", ""), ("old", "1", "Breaks: libfoo (>= 2)\n")]);

        let packages = resolve(&universe, &installed, &want(&["tool"])).unwrap();

        assert_eq!(names(&packages), ["libfoo", "old", "tool"]);
    }

    #[test]
    fn conflicts_reach_the_providers_of_a_virtual_name() {
        let snapshot = [stanza("tool", "1", "Conflicts: awk\n")];
        let universe = universe(&[("debian", &snapshot)]);
        let installed = status(&[("mawk", "1", "Provides: awk\n")]);

        let error = resolve(&universe, &installed, &want(&["tool"])).unwrap_err();

        assert!(
            error.contains("tool 1 breaks or conflicts with mawk 1"),
            "{error}"
        );
    }

    #[test]
    fn a_snapshot_older_than_the_image_is_not_a_downgrade() {
        let snapshot = [
            stanza("libc6-dev", "1", "Depends: libc6 (= 1)\n"),
            stanza("libc6", "1", ""),
        ];
        let universe = universe(&[("debian", &snapshot)]);
        let installed = status(&[("libc6", "2", "")]);

        let error = resolve(&universe, &installed, &want(&["libc6-dev"])).unwrap_err();

        assert!(error.contains("move the snapshot later"), "{error}");
    }

    #[test]
    fn providers_come_from_the_version_that_won() {
        let old = [stanza("p", "1", "Provides: x\n")];
        let new = [stanza("p", "2", "")];
        let universe = universe(&[("debian", &old), ("debian-security", &new)]);

        let error = resolve(&universe, &BTreeMap::new(), &want(&["x"])).unwrap_err();

        assert!(error.contains("provides x"), "{error}");
    }

    #[test]
    fn versioned_provides_satisfy_versioned_virtual_dependencies() {
        let snapshot = [
            stanza("tool", "1", "Depends: virt (>= 1)\n"),
            stanza("too-old", "1", "Provides: virt (= 0.5)\n"),
            stanza("unversioned", "1", "Provides: virt\n"),
            stanza("versioned", "1", "Provides: virt (= 1.5)\n"),
        ];
        let universe = universe(&[("debian", &snapshot)]);

        let packages = resolve(&universe, &BTreeMap::new(), &want(&["tool"])).unwrap();

        assert_eq!(names(&packages), ["tool", "versioned"]);
    }

    #[test]
    fn a_foreign_architecture_qualifier_is_not_satisfiable() {
        let snapshot = [
            stanza("tool", "1", "Depends: lib:i386\n"),
            stanza("lib", "1", ""),
        ];
        let universe = universe(&[("debian", &snapshot)]);

        let error = resolve(&universe, &BTreeMap::new(), &want(&["tool"])).unwrap_err();

        assert!(error.contains("foreign architecture"), "{error}");
    }

    #[test]
    fn a_selected_alternative_at_the_wrong_version_falls_through() {
        let snapshot = [
            stanza("a", "1", "Depends: lib (>= 2) | other\n"),
            stanza("lib", "1", ""),
            stanza("other", "1", ""),
            stanza("needs-lib", "1", "Depends: lib\n"),
        ];
        let universe = universe(&[("debian", &snapshot)]);

        let packages = resolve(&universe, &BTreeMap::new(), &want(&["needs-lib", "a"])).unwrap();

        assert_eq!(names(&packages), ["a", "lib", "needs-lib", "other"]);
    }

    #[test]
    fn held_packages_count_as_installed() {
        let held = parse_status(
            "Package: libc6\nStatus: hold ok installed\nVersion: 2\n\n\
             Package: gone\nStatus: purge ok not-installed\nVersion: 1\n",
        )
        .unwrap();
        assert_eq!(held.keys().collect::<Vec<_>>(), ["libc6"]);
    }

    fn ustar_entry(prefix: &str, name: &str, body: &[u8]) -> Vec<u8> {
        let mut entry = tar_entry(name, b'0', body);
        entry[257..263].copy_from_slice(b"ustar\0");
        entry[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
        entry
    }

    fn pax_record(key: &str, value: &str) -> String {
        let body = format!(" {key}={value}\n");
        let mut length = body.len() + 1;
        while format!("{length}{body}").len() != length {
            length += 1;
        }
        format!("{length}{body}")
    }

    #[test]
    fn tar_names_follow_ustar_gnu_and_pax_and_the_last_entry_wins() {
        let ustar = ustar_entry("var/lib/dpkg", "status", b"ustar");
        assert_eq!(
            tar_member(&[ustar, vec![0; 1024]].concat(), STATUS_PATH),
            Some(&b"ustar"[..])
        );

        let mut gnu = tar_entry("var/lib/dpkg/status", b'0', b"gnu");
        gnu[257..265].copy_from_slice(b"ustar  \0");
        gnu[345..350].copy_from_slice(b"12345");
        assert_eq!(
            tar_member(&[gnu, vec![0; 1024]].concat(), STATUS_PATH),
            Some(&b"gnu"[..])
        );

        let long = [
            tar_entry("././@LongLink", b'L', b"var/lib/dpkg/status\0"),
            tar_entry("var/lib/dpkg/stat", b'0', b"long"),
            vec![0; 1024],
        ]
        .concat();
        assert_eq!(tar_member(&long, STATUS_PATH), Some(&b"long"[..]));

        let mut records = pax_record("path", STATUS_PATH) + &pax_record("size", "3");
        records.push_str(&pax_record(
            "SCHILY.xattr.security.capability",
            "\u{1}\u{2}",
        ));
        let mut records = records.into_bytes();
        let binary = records.len() - 3;
        records[binary] = 0xff;
        let pax = [
            tar_entry("PaxHeaders/x", b'x', &records),
            tar_entry("short-name", b'0', b"paxXXXXX"),
            vec![0; 1024],
        ]
        .concat();
        assert_eq!(tar_member(&pax, STATUS_PATH), Some(&b"pax"[..]));

        let link = [
            tar_entry("././@LongLink", b'L', b"var/lib/dpkg/status\0"),
            tar_entry("././@LongLink", b'K', b"some/long/link/target\0"),
            tar_entry(b"var/lib/dpkg/st\xc3", b'0', b"kept"),
            vec![0; 1024],
        ]
        .concat();
        assert_eq!(tar_member(&link, STATUS_PATH), Some(&b"kept"[..]));

        let twice = [
            tar_entry("./var/lib/dpkg/status", b'0', b"first"),
            tar_entry("./var/lib/dpkg/status", b'0', b"second"),
            vec![0; 1024],
        ]
        .concat();
        assert_eq!(tar_member(&twice, STATUS_PATH), Some(&b"second"[..]));

        let replaced = [
            tar_entry("./var/lib/dpkg/status", b'0', b"file"),
            tar_entry("./var/lib/dpkg/status", b'2', b""),
            vec![0; 1024],
        ]
        .concat();
        assert_eq!(tar_member(&replaced, STATUS_PATH), None);
    }

    fn json(text: &str) -> td_engine::json::Json {
        td_engine::json::parse(text).unwrap()
    }

    #[test]
    fn the_index_selects_exactly_one_linux_amd64_manifest() {
        let index = json(
            r#"{"manifests":[
              {"digest":"sha256:a","platform":{"os":"linux","architecture":"arm64","variant":"v8"}},
              {"digest":"sha256:b","platform":{"os":"linux","architecture":"amd64"}},
              {"digest":"sha256:c","platform":{"os":"unknown","architecture":"unknown"}}]}"#,
        );
        assert_eq!(
            amd64_manifest_digest(&index).unwrap().as_deref(),
            Some("sha256:b")
        );
        assert_eq!(
            amd64_manifest_digest(&json(r#"{"layers":[]}"#)).unwrap(),
            None
        );
        let two = json(
            r#"{"manifests":[{"digest":"sha256:b","platform":{"os":"linux","architecture":"amd64"}},
              {"digest":"sha256:d","platform":{"os":"linux","architecture":"amd64"}}]}"#,
        );
        assert!(amd64_manifest_digest(&two).is_err());
    }

    #[test]
    fn only_a_single_gzip_layer_is_accepted() {
        let hex = "e".repeat(64);
        let manifest = |media: &str, layers: &str| {
            json(&format!(
                r#"{{"mediaType":"application/vnd.oci.image.manifest.v1+json","layers":[{}]}}"#,
                layers.replace("MEDIA", media)
            ))
        };
        let layer = format!(r#"{{"mediaType":"MEDIA","digest":"sha256:{hex}","size":42}}"#);
        let gzip = "application/vnd.oci.image.layer.v1.tar+gzip";
        assert_eq!(
            single_gzip_layer(&manifest(gzip, &layer)).unwrap(),
            (format!("sha256:{hex}"), 42)
        );
        let zstd = single_gzip_layer(&manifest(
            "application/vnd.oci.image.layer.v1.tar+zstd",
            &layer,
        ));
        assert!(zstd.unwrap_err().contains("gzip layers only"));
        let two = single_gzip_layer(&manifest(gzip, &format!("{layer},{layer}")));
        assert!(two.unwrap_err().contains("2 layers"));
    }

    const SPEC: &str = "# header comment\n\
                        image library/debian:trixie-20261005-slim\n\
                        snapshot 20261005T000000Z\n\
                        archive debian trixie\n\
                        archive debian-security trixie-security\n\
                        want gcc make\n\
                        \n\
                        # resolved by `td-feed debian-root resolve`; review the diff\n\
                        deb debian-root-old old_1_amd64.deb 00 1 https://x/old 1\n";

    #[test]
    fn the_spec_reads_input_lines_and_ignores_resolved_ones() {
        let spec = parse_spec(SPEC).unwrap();
        assert_eq!(spec.repository, "library/debian");
        assert_eq!(spec.tag, "trixie-20261005-slim");
        assert_eq!(spec.snapshot, "20261005T000000Z");
        assert_eq!(spec.archives.len(), 2);
        assert_eq!(spec.want, ["gcc", "make"]);
        for broken in [
            SPEC.replace("snapshot 20261005T000000Z", "snapshot yesterday"),
            SPEC.replace("want gcc make", "want gcc Make"),
            SPEC.replace("image library/debian:trixie-20261005-slim", "image debian"),
            format!("{SPEC}mystery line\n"),
        ] {
            assert!(parse_spec(&broken).is_err(), "{broken}");
        }
    }

    #[test]
    fn rendering_replaces_resolved_lines_and_keeps_inputs() {
        let spec = parse_spec(SPEC).unwrap();
        let mut universe = Universe::default();
        universe
            .add_index("debian", &package("gcc", "14.2", "", ""))
            .unwrap();
        let resolved = Resolved {
            index: "sha256:i".into(),
            manifest: "sha256:m".into(),
            layer_hex: "a".repeat(64),
            layer_size: 7,
            inreleases: vec![("debian".into(), "trixie".into(), "f".repeat(64))],
            packages: universe.packages.into_values().collect(),
        };
        let text = render(SPEC, &spec, &resolved).unwrap();
        assert!(text.starts_with("# header comment\nimage library/debian:trixie-20261005-slim\n"));
        assert!(!text.contains("debian-root-old"));
        assert_eq!(text.matches("# resolved by").count(), 1);
        assert!(
            text.contains("layer debian-root-layer debian-trixie-20261005-slim-amd64-layer.tar.gz")
        );
        assert!(text.contains(&format!(
            "deb debian-root-gcc gcc_14.2_amd64.deb {} 10 \
             https://snapshot.debian.org/archive/debian/20261005T000000Z/\
             pool/main/x/gcc/gcc_14.2_amd64.deb 14.2\n",
            "0".repeat(64)
        )));
        assert_eq!(parse_spec(&text).unwrap(), spec);
        assert_eq!(render(&text, &spec, &resolved).unwrap(), text);
    }
}
