use crate::json;
use crate::state::Frame;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

const MAX_INDEX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_INDEX_ENTRIES: usize = 200_000;
const MAX_SECTIONS: usize = 65_535;
const MAX_STRING_TABLE: u64 = 32 * 1024 * 1024;
const MAX_SYMBOLS: usize = 1_000_000;
const MAX_NOTE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CACHED_SYMBOL_TABLES: usize = 4;
const MAX_CACHED_SYMBOL_BYTES: usize = 128 * 1024 * 1024;
const MAX_OBJECT_DEBUG_BYTES: usize = 128 * 1024 * 1024;
const MAX_OBJECT_STATE_BYTES: usize = 128 * 1024 * 1024;
const MAX_OBJECT_SEGMENTS: usize = 1_000_000;
const MAX_SYMBOL_ERRORS: usize = 4096;
const MAX_SYMBOL_ERROR_BYTES: usize = 4096;

#[derive(Clone, Debug)]
pub struct Resolved {
    pub function: Vec<u8>,
    pub object: Vec<u8>,
    pub debug: Vec<u8>,
    pub build_id: Vec<u8>,
    pub provenance: Vec<u8>,
    pub object_address: u64,
    pub function_address: u64,
    pub assembly_boundary: bool,
    pub source: Option<crate::dwarf::Location>,
}

#[derive(Default)]
pub struct Symbolizer {
    objects: BTreeMap<(u32, u32, u64, u64), Object>,
    pub errors: Vec<String>,
    omitted_errors: u64,
    persistent_error_count: usize,
    persistent_omitted_errors: u64,
    use_clock: u64,
}

struct Object {
    runtime: PathBuf,
    debug: PathBuf,
    build_id: Vec<u8>,
    provenance: Vec<u8>,
    segments: Vec<Segment>,
    symbols: Option<Result<Symbols, String>>,
    last_used: u64,
    used: bool,
    line_error_reported: bool,
}

#[derive(Clone)]
struct Segment {
    offset: u64,
    filesz: u64,
    vaddr: u64,
}

#[derive(Clone)]
struct Symbol {
    value: u64,
    size: u64,
    name_at: usize,
}

struct Symbols {
    entries: Vec<Symbol>,
    max_ends: Vec<u64>,
    strings: Vec<u8>,
    lines: Option<crate::dwarf::Lines>,
    line_error: Option<String>,
}

impl Symbols {
    fn heap_bytes(&self) -> usize {
        self.entries
            .capacity()
            .saturating_mul(std::mem::size_of::<Symbol>())
            .saturating_add(
                self.max_ends
                    .capacity()
                    .saturating_mul(std::mem::size_of::<u64>()),
            )
            .saturating_add(self.strings.capacity())
            .saturating_add(
                self.lines
                    .as_ref()
                    .map(crate::dwarf::Lines::heap_bytes)
                    .unwrap_or_default(),
            )
            .saturating_add(self.line_error.as_ref().map_or(0, |error| error.capacity()))
    }
}

impl Symbolizer {
    pub fn from_index(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        match load_index(path) {
            Ok(value) => value,
            Err(error) => Self {
                objects: BTreeMap::new(),
                errors: vec![error],
                omitted_errors: 0,
                persistent_error_count: 1,
                persistent_omitted_errors: 0,
                use_clock: 0,
            },
        }
    }

    pub fn unavailable(error: String) -> Self {
        Self {
            objects: BTreeMap::new(),
            errors: vec![error],
            omitted_errors: 0,
            persistent_error_count: 1,
            persistent_omitted_errors: 0,
            use_clock: 0,
        }
    }

    pub fn begin_report(&mut self) {
        self.errors.truncate(self.persistent_error_count);
        self.omitted_errors = self.persistent_omitted_errors;
        for object in self.objects.values_mut() {
            object.used = false;
            object.line_error_reported = false;
            if object.symbols.as_ref().is_some_and(Result::is_err) {
                object.symbols = None;
            }
        }
    }

    pub fn omitted_errors(&self) -> u64 {
        self.omitted_errors
    }

    pub fn resolve(&mut self, frame: &Frame, max_expansion: usize) -> Option<Resolved> {
        let key = (frame.major, frame.minor, frame.inode, 0);
        if !self.objects.contains_key(&key) {
            self.record_error(format!(
                "no indexed object matches device {}:{} inode {} generation {}",
                frame.major, frame.minor, frame.inode, frame.inode_generation
            ));
            return None;
        }
        self.use_clock = self.use_clock.saturating_add(1);
        let use_clock = self.use_clock;
        if self.objects.get(&key)?.symbols.is_none() {
            let debug = self.objects.get(&key)?.debug.clone();
            let mut loaded = load_symbols(&debug);
            let reserve = loaded.as_ref().map(Symbols::heap_bytes).unwrap_or_default();
            if reserve > MAX_CACHED_SYMBOL_BYTES {
                loaded = Err(format!(
                    "symbol table expands beyond {MAX_CACHED_SYMBOL_BYTES} bytes"
                ));
            }
            self.evict_symbol_tables(&key, reserve);
            let symbol_error = loaded
                .as_ref()
                .err()
                .map(|error| format!("read indexed debug symbols {}: {error}", debug.display()));
            self.objects.get_mut(&key)?.symbols = Some(loaded);
            if let Some(error) = symbol_error {
                self.record_error(error);
                return None;
            }
        }
        let (resolved, errors) =
            resolve_loaded(self.objects.get_mut(&key)?, frame, max_expansion, use_clock);
        for error in errors {
            self.record_error(error);
        }
        resolved
    }

    fn evict_symbol_tables(&mut self, keep: &(u32, u32, u64, u64), reserve: usize) {
        while self
            .objects
            .values()
            .filter(|object| matches!(object.symbols.as_ref(), Some(Ok(_))))
            .count()
            >= MAX_CACHED_SYMBOL_TABLES
            || self
                .objects
                .values()
                .filter_map(|object| {
                    object
                        .symbols
                        .as_ref()
                        .and_then(|symbols| symbols.as_ref().ok())
                })
                .fold(reserve, |total, symbols| {
                    total.saturating_add(symbols.heap_bytes())
                })
                > MAX_CACHED_SYMBOL_BYTES
        {
            let candidate = self
                .objects
                .iter()
                .filter(|(key, object)| {
                    *key != keep && matches!(object.symbols.as_ref(), Some(Ok(_)))
                })
                .min_by_key(|(key, object)| (object.last_used, *key))
                .map(|(key, _)| *key);
            let Some(candidate) = candidate else {
                break;
            };
            if let Some(object) = self.objects.get_mut(&candidate) {
                object.symbols = None;
            }
        }
    }

    fn record_error(&mut self, mut error: String) {
        if self.errors.len() >= MAX_SYMBOL_ERRORS {
            self.omitted_errors = self.omitted_errors.saturating_add(1);
            return;
        }
        if error.len() > MAX_SYMBOL_ERROR_BYTES {
            let mut end = MAX_SYMBOL_ERROR_BYTES;
            while !error.is_char_boundary(end) {
                end = end.saturating_sub(1);
            }
            error.truncate(end);
        }
        if self.errors.last() != Some(&error) {
            self.errors.push(error);
        }
    }

    pub fn identities_json(&self, limit: usize) -> Result<String, String> {
        let mut rows = Vec::new();
        let mut bytes = 0usize;
        for ((major, minor, inode, generation), object) in
            self.objects.iter().filter(|(_, object)| object.used)
        {
            let field_bytes = object
                .runtime
                .as_os_str()
                .as_encoded_bytes()
                .len()
                .checked_add(object.debug.as_os_str().as_encoded_bytes().len())
                .and_then(|value| value.checked_add(object.build_id.len()))
                .and_then(|value| value.checked_add(object.provenance.len()))
                .ok_or("object identity expansion overflow")?;
            let worst_case = field_bytes
                .checked_mul(8)
                .and_then(|value| value.checked_add(512))
                .ok_or("object identity expansion overflow")?;
            if bytes
                .checked_add(worst_case)
                .map(|value| value > limit)
                .unwrap_or(true)
            {
                return Err(format!(
                    "object identities expand beyond {limit} report bytes"
                ));
            }
            let row = format!(
                "{{\"device_major\":{major},\"device_minor\":{minor},\"inode\":{inode},\
                 \"inode_generation\":{generation},{},{},\"build_id\":\"{}\",{}}}",
                json::named_bytes("runtime", object.runtime.as_os_str().as_encoded_bytes()),
                json::named_bytes("debug", object.debug.as_os_str().as_encoded_bytes()),
                json::hex(&object.build_id),
                json::named_bytes("provenance", &object.provenance)
            );
            bytes = bytes
                .checked_add(row.len())
                .and_then(|value| value.checked_add(usize::from(!rows.is_empty())))
                .ok_or("object identity expansion overflow")?;
            if bytes > limit {
                return Err(format!(
                    "object identities expand beyond {limit} report bytes"
                ));
            }
            rows.push(row);
        }
        Ok(rows.join(","))
    }
}

fn resolve_loaded(
    object: &mut Object,
    frame: &Frame,
    max_expansion: usize,
    use_clock: u64,
) -> (Option<Resolved>, Vec<String>) {
    object.last_used = use_clock;
    let mut errors = Vec::new();
    let Some(file_offset) = frame.relative else {
        return (None, errors);
    };
    let address = object
        .segments
        .iter()
        .find(|segment| {
            file_offset >= segment.offset
                && file_offset < segment.offset.saturating_add(segment.filesz)
        })
        .and_then(|segment| {
            file_offset
                .checked_sub(segment.offset)
                .and_then(|within| segment.vaddr.checked_add(within))
        })
        .unwrap_or(file_offset);
    let Some(Ok(symbols)) = object.symbols.as_ref() else {
        return (None, errors);
    };
    if !object.line_error_reported {
        if let Some(error) = &symbols.line_error {
            errors.push(format!(
                "read indexed debug lines {}: {error}",
                object.debug.display()
            ));
        }
        object.line_error_reported = true;
    }
    let symbol = covering_symbol(symbols, address);
    let function = symbol
        .and_then(|symbol| c_string(&symbols.strings, symbol.name_at).ok())
        .filter(|name| !name.is_empty());
    let Some(function) = function else {
        errors.push(format!(
            "no function symbol covers object address 0x{address:x} in {}",
            object.runtime.display()
        ));
        return (None, errors);
    };
    let Some(function_address) = symbol.map(|symbol| symbol.value) else {
        return (None, errors);
    };
    let source = symbols
        .lines
        .as_ref()
        .and_then(|lines| lines.resolve(address));
    let runtime = object.runtime.as_os_str().as_encoded_bytes();
    let debug = object.debug.as_os_str().as_encoded_bytes();
    let required = [
        function.len(),
        runtime.len(),
        debug.len(),
        object.build_id.len(),
        object.provenance.len(),
        source.as_ref().map_or(0, |location| location.file.len()),
    ]
    .into_iter()
    .try_fold(std::mem::size_of::<Resolved>(), usize::checked_add);
    let resolved = required
        .filter(|required| *required <= max_expansion)
        .map(|_| Resolved {
            function: function.to_vec(),
            object: runtime.to_vec(),
            debug: debug.to_vec(),
            build_id: object.build_id.clone(),
            provenance: object.provenance.clone(),
            object_address: address,
            function_address,
            assembly_boundary: has_assembly_boundary(&object.provenance),
            source: source.map(|location| crate::dwarf::Location {
                file: location.file.to_vec(),
                line: location.line,
                column: location.column,
                discriminator: location.discriminator,
            }),
        });
    object.used |= resolved.is_some();
    if resolved.is_none() {
        errors.push(format!(
            "resolved symbol expansion exceeds the remaining {max_expansion}-byte report budget"
        ));
    }
    (resolved, errors)
}

fn covering_symbol(symbols: &Symbols, address: u64) -> Option<&Symbol> {
    let mut at = symbols
        .entries
        .partition_point(|symbol| symbol.value <= address)
        .checked_sub(1)?;
    loop {
        let symbol = symbols.entries.get(at)?;
        if (symbol.size == 0 && address == symbol.value)
            || (symbol.size != 0 && address < symbol.value.saturating_add(symbol.size))
        {
            return Some(symbol);
        }
        if at == 0 || symbols.max_ends.get(at.checked_sub(1)?)? <= &address {
            return None;
        }
        at = at.saturating_sub(1);
    }
}

fn has_assembly_boundary(provenance: &[u8]) -> bool {
    provenance
        .split(|byte| *byte == b';')
        .any(|field| field == b"assembly-boundary=1")
}

fn load_index(path: &Path) -> Result<Symbolizer, String> {
    let metadata = path
        .symlink_metadata()
        .map_err(|e| format!("lstat object index {}: {e}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "object index is not a regular file: {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_INDEX_BYTES {
        return Err(format!("object index exceeds {MAX_INDEX_BYTES} bytes"));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("read object index {}: {e}", path.display()))?;
    let mut lines = text.lines();
    if lines.next() != Some("td-profiler-objects-v1") {
        return Err("object index has unsupported header".into());
    }
    let mut objects: BTreeMap<(u32, u32, u64, u64), Object> = BTreeMap::new();
    let mut errors = Vec::new();
    let mut omitted_errors = 0u64;
    let mut previous = None::<Vec<u8>>;
    let mut object_state_bytes = 0usize;
    let mut object_segments = 0usize;
    for (number, line) in lines.enumerate() {
        if number >= MAX_INDEX_ENTRIES {
            return Err(format!("object index exceeds {MAX_INDEX_ENTRIES} entries"));
        }
        if previous
            .as_deref()
            .map(|prior| prior >= line.as_bytes())
            .unwrap_or(false)
        {
            return Err(format!(
                "object index line {} is not in strict byte order",
                number + 2
            ));
        }
        previous = Some(line.as_bytes().to_vec());
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 4 {
            return Err(format!(
                "object index line {} has {} fields",
                number + 2,
                fields.len()
            ));
        }
        let runtime = PathBuf::from(fields.first().copied().unwrap_or_default());
        let debug = PathBuf::from(fields.get(1).copied().unwrap_or_default());
        if !store_path(&runtime) || !store_path(&debug) {
            return Err(format!("object index line {} leaves /td/store", number + 2));
        }
        let build_id = decode_hex(fields.get(2).copied().unwrap_or_default())?;
        if build_id.len() != 20 {
            return Err(format!(
                "object index line {} build ID is not SHA-1",
                number + 2
            ));
        }
        let provenance = fields
            .get(3)
            .copied()
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        if provenance.is_empty() || provenance.len() > 4096 {
            return Err(format!(
                "object index line {} has invalid provenance length",
                number + 2
            ));
        }
        let metadata = match runtime.symlink_metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => {
                record_index_error(
                    &mut errors,
                    &mut omitted_errors,
                    format!("indexed runtime is not a file: {}", runtime.display()),
                );
                continue;
            }
            Err(error) => {
                record_index_error(
                    &mut errors,
                    &mut omitted_errors,
                    format!("stat indexed runtime {}: {error}", runtime.display()),
                );
                continue;
            }
        };
        match debug.symlink_metadata() {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                record_index_error(
                    &mut errors,
                    &mut omitted_errors,
                    format!("indexed debug companion is not a file: {}", debug.display()),
                );
                continue;
            }
            Err(error) => {
                record_index_error(
                    &mut errors,
                    &mut omitted_errors,
                    format!("stat indexed debug companion {}: {error}", debug.display()),
                );
                continue;
            }
        }
        let runtime_id = match load_build_id(&runtime) {
            Ok(id) => id,
            Err(error) => {
                record_index_error(
                    &mut errors,
                    &mut omitted_errors,
                    format!(
                        "read indexed runtime build ID {}: {error}",
                        runtime.display()
                    ),
                );
                continue;
            }
        };
        let debug_id = match load_build_id(&debug) {
            Ok(id) => id,
            Err(error) => {
                record_index_error(
                    &mut errors,
                    &mut omitted_errors,
                    format!("read indexed debug build ID {}: {error}", debug.display()),
                );
                continue;
            }
        };
        if runtime_id != build_id || debug_id != build_id {
            record_index_error(
                &mut errors,
                &mut omitted_errors,
                format!(
                    "indexed build ID {} does not match runtime {} ({}) and debug {} ({})",
                    json::hex(&build_id),
                    runtime.display(),
                    json::hex(&runtime_id),
                    debug.display(),
                    json::hex(&debug_id)
                ),
            );
            continue;
        }
        let device = metadata.dev();
        let key = (linux_major(device), linux_minor(device), metadata.ino(), 0);
        if let Some(existing) = objects.get(&key) {
            if existing.build_id != build_id || existing.provenance != provenance {
                return Err(format!(
                    "object index gives conflicting metadata for hard-linked identity {key:?}"
                ));
            }
            continue;
        }
        let segments = match load_segments(&runtime) {
            Ok(segments) => segments,
            Err(error) => {
                record_index_error(
                    &mut errors,
                    &mut omitted_errors,
                    format!("read indexed runtime {}: {error}", runtime.display()),
                );
                continue;
            }
        };
        object_segments = object_segments
            .checked_add(segments.len())
            .ok_or("object segment count overflow")?;
        if object_segments > MAX_OBJECT_SEGMENTS {
            return Err(format!(
                "object inventory exceeds {MAX_OBJECT_SEGMENTS} segments"
            ));
        }
        let object_bytes = std::mem::size_of::<Object>()
            .checked_add(runtime.as_os_str().as_encoded_bytes().len())
            .and_then(|value| value.checked_add(debug.as_os_str().as_encoded_bytes().len()))
            .and_then(|value| value.checked_add(build_id.len()))
            .and_then(|value| value.checked_add(provenance.len()))
            .and_then(|value| {
                value.checked_add(
                    segments
                        .capacity()
                        .saturating_mul(std::mem::size_of::<Segment>()),
                )
            })
            .ok_or("object inventory byte count overflow")?;
        object_state_bytes = object_state_bytes
            .checked_add(object_bytes)
            .ok_or("object inventory byte count overflow")?;
        if object_state_bytes > MAX_OBJECT_STATE_BYTES {
            return Err(format!(
                "object inventory expands beyond {MAX_OBJECT_STATE_BYTES} bytes"
            ));
        }
        objects.insert(
            key,
            Object {
                runtime,
                debug,
                build_id,
                provenance,
                segments,
                symbols: None,
                last_used: 0,
                used: false,
                line_error_reported: false,
            },
        );
    }
    let persistent_error_count = errors.len();
    Ok(Symbolizer {
        objects,
        errors,
        omitted_errors: 0,
        persistent_error_count,
        persistent_omitted_errors: omitted_errors,
        use_clock: 0,
    })
}

fn record_index_error(errors: &mut Vec<String>, omitted: &mut u64, mut error: String) {
    if errors.len() >= MAX_SYMBOL_ERRORS {
        *omitted = omitted.saturating_add(1);
        return;
    }
    if error.len() > MAX_SYMBOL_ERROR_BYTES {
        let mut end = MAX_SYMBOL_ERROR_BYTES;
        while !error.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        error.truncate(end);
    }
    errors.push(error);
}

fn store_path(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::RootDir))
        && matches!(components.next(), Some(Component::Normal(value)) if value == "td")
        && matches!(components.next(), Some(Component::Normal(value)) if value == "store")
        && matches!(components.next(), Some(Component::Normal(_)))
        && components.all(|component| matches!(component, Component::Normal(_)))
}

pub(crate) fn load_build_id(path: &Path) -> Result<Vec<u8>, String> {
    let mut file = File::open(path).map_err(|e| format!("open ELF: {e}"))?;
    let length = file.metadata().map_err(|e| format!("stat ELF: {e}"))?.len();
    let header = read_at(&mut file, 0, 64)?;
    require_elf64(&header)?;
    let shoff = le64(&header, 40)?;
    let shentsize = usize::from(le16(&header, 58)?);
    let shnum = usize::from(le16(&header, 60)?);
    if shentsize != 64 || shnum == 0 || shnum > MAX_SECTIONS {
        return Err("ELF section-header shape is unsupported".into());
    }
    let mut ids = Vec::new();
    for index in 0..shnum {
        let offset = shoff
            .checked_add((index.saturating_mul(shentsize)) as u64)
            .ok_or("ELF section-header offset overflow")?;
        let section = read_at(&mut file, offset, shentsize)?;
        if le32(&section, 4)? != 7 {
            continue;
        }
        let note_offset = le64(&section, 24)?;
        let note_size = le64(&section, 32)?;
        if note_size > MAX_NOTE_BYTES {
            return Err(format!("ELF note section exceeds {MAX_NOTE_BYTES} bytes"));
        }
        let note_end = note_offset
            .checked_add(note_size)
            .ok_or("ELF note section range overflow")?;
        if note_end > length {
            return Err("ELF note section runs past the file".into());
        }
        let notes = read_at(
            &mut file,
            note_offset,
            usize::try_from(note_size).map_err(|_| "ELF note section does not fit memory")?,
        )?;
        let mut cursor = 0u64;
        while cursor < note_size {
            let header_end = cursor
                .checked_add(12)
                .ok_or("ELF note header offset overflow")?;
            if header_end > note_size {
                return Err("ELF note section ends in a partial header".into());
            }
            let note = notes
                .get(
                    usize::try_from(cursor).map_err(|_| "ELF note offset does not fit memory")?
                        ..usize::try_from(header_end)
                            .map_err(|_| "ELF note offset does not fit memory")?,
                )
                .ok_or("ELF note header slice overflow")?;
            let namesz = u64::from(le32(note, 0)?);
            let descsz = u64::from(le32(note, 4)?);
            let note_type = le32(note, 8)?;
            let name_start = header_end;
            let name_end = name_start
                .checked_add(namesz)
                .ok_or("ELF note name range overflow")?;
            let desc_start = align4(name_end)?;
            let desc_end = desc_start
                .checked_add(descsz)
                .ok_or("ELF note descriptor range overflow")?;
            let next = align4(desc_end)?;
            if next > note_size {
                return Err("ELF note runs past its section".into());
            }
            if note_type == 3 && namesz == 4 {
                let name = notes
                    .get(
                        usize::try_from(name_start)
                            .map_err(|_| "ELF note name does not fit memory")?
                            ..usize::try_from(name_end)
                                .map_err(|_| "ELF note name does not fit memory")?,
                    )
                    .ok_or("ELF note name slice overflow")?;
                if name == b"GNU\0" {
                    if descsz != 20 {
                        return Err(format!(
                            "GNU build ID is {descsz} bytes, expected SHA-1's 20"
                        ));
                    }
                    ids.push(
                        notes
                            .get(
                                usize::try_from(desc_start)
                                    .map_err(|_| "ELF note descriptor does not fit memory")?
                                    ..usize::try_from(desc_end)
                                        .map_err(|_| "ELF note descriptor does not fit memory")?,
                            )
                            .ok_or("ELF note descriptor slice overflow")?
                            .to_vec(),
                    );
                }
            }
            cursor = next;
        }
    }
    if ids.len() != 1 {
        return Err(format!(
            "expected exactly one GNU build ID, found {}",
            ids.len()
        ));
    }
    ids.pop()
        .ok_or_else(|| "GNU build ID vanished after count check".into())
}

pub(crate) fn is_runtime_elf(path: &Path) -> Result<bool, String> {
    let mut file = File::open(path).map_err(|e| format!("open ELF candidate: {e}"))?;
    let mut header = [0u8; 64];
    let read = file
        .read(&mut header)
        .map_err(|e| format!("read ELF candidate: {e}"))?;
    if header.get(..4) != Some(b"\x7fELF") {
        return Ok(false);
    }
    if read < header.len() {
        return Err("ELF candidate has a truncated header".into());
    }
    require_elf64(&header)?;
    Ok(matches!(le16(&header, 16)?, 2 | 3))
}

fn align4(value: u64) -> Result<u64, String> {
    value
        .checked_add(3)
        .map(|value| value & !3)
        .ok_or_else(|| "ELF note alignment overflow".into())
}

fn linux_major(device: u64) -> u32 {
    (((device >> 8) & 0x0fff) | ((device >> 32) & 0xffff_f000)) as u32
}

fn linux_minor(device: u64) -> u32 {
    ((device & 0x00ff) | ((device >> 12) & 0xffff_ff00)) as u32
}

fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(text.len() / 2);
    let mut at = 0usize;
    while at < text.len() {
        let pair = text
            .as_bytes()
            .get(at..at.saturating_add(2))
            .ok_or("hex field has odd length")?;
        let high = nibble(*pair.first().ok_or("missing hex byte")?)?;
        let low = nibble(*pair.get(1).ok_or("missing hex byte")?)?;
        out.push((high << 4) | low);
        at = at.saturating_add(2);
    }
    Ok(out)
}

fn nibble(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err("hex field is not lowercase hexadecimal".into()),
    }
}

fn load_segments(path: &Path) -> Result<Vec<Segment>, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let header = read_at(&mut file, 0, 64)?;
    require_elf64(&header)?;
    let phoff = le64(&header, 32)?;
    let phentsize = usize::from(le16(&header, 54)?);
    let phnum = usize::from(le16(&header, 56)?);
    if phentsize != 56 || phnum > 4096 {
        return Err("ELF program-header shape is unsupported".into());
    }
    let mut segments = Vec::new();
    for index in 0..phnum {
        let offset = phoff
            .checked_add((index.saturating_mul(phentsize)) as u64)
            .ok_or("ELF program-header offset overflow")?;
        let entry = read_at(&mut file, offset, phentsize)?;
        if le32(&entry, 0)? == 1 {
            segments.push(Segment {
                offset: le64(&entry, 8)?,
                vaddr: le64(&entry, 16)?,
                filesz: le64(&entry, 32)?,
            });
        }
    }
    Ok(segments)
}

fn load_symbols(path: &Path) -> Result<Symbols, String> {
    let mut file = File::open(path).map_err(|e| format!("open debug companion: {e}"))?;
    let length = file
        .metadata()
        .map_err(|e| format!("stat debug companion: {e}"))?
        .len();
    let header = read_at(&mut file, 0, 64)?;
    require_elf64(&header)?;
    let shoff = le64(&header, 40)?;
    let shentsize = usize::from(le16(&header, 58)?);
    let shnum = usize::from(le16(&header, 60)?);
    let shstrndx = usize::from(le16(&header, 62)?);
    if shentsize != 64 || shnum == 0 || shnum > MAX_SECTIONS {
        return Err("ELF section-header shape is unsupported".into());
    }
    let mut sections = Vec::with_capacity(shnum);
    for index in 0..shnum {
        let offset = shoff
            .checked_add((index.saturating_mul(shentsize)) as u64)
            .ok_or("ELF section-header offset overflow")?;
        let entry = read_at(&mut file, offset, shentsize)?;
        sections.push(Section {
            name_at: le32(&entry, 0)?,
            kind: le32(&entry, 4)?,
            flags: le64(&entry, 8)?,
            offset: le64(&entry, 24)?,
            size: le64(&entry, 32)?,
            link: le32(&entry, 40)?,
            entsize: le64(&entry, 56)?,
        });
    }
    let shstrtab = sections
        .get(shstrndx)
        .ok_or("ELF has an invalid section-name string-table index")?;
    if shstrtab.kind != 3 || shstrtab.size > MAX_STRING_TABLE {
        return Err("ELF section-name string table is unsupported".into());
    }
    let section_names = read_section(&mut file, shstrtab, length, "section-name string table")?;
    let symtab = named_section(&sections, &section_names, b".symtab")?
        .filter(|section| section.kind == 2)
        .ok_or("debug companion has no symbol table")?;
    if symtab.entsize != 24 || symtab.size % 24 != 0 {
        return Err("ELF symbol table has unsupported entry size".into());
    }
    let count = usize::try_from(symtab.size / 24).map_err(|_| "symbol count overflow")?;
    if count > MAX_SYMBOLS {
        return Err(format!("ELF symbol table exceeds {MAX_SYMBOLS} entries"));
    }
    let strtab = sections
        .get(usize::try_from(symtab.link).map_err(|_| "string-table index overflow")?)
        .ok_or("ELF symbol table has invalid string-table link")?;
    if strtab.kind != 3 || strtab.size > MAX_STRING_TABLE {
        return Err(format!("ELF string table exceeds {MAX_STRING_TABLE} bytes"));
    }
    let strings = read_section(&mut file, strtab, length, "symbol string table")?;
    let symbol_bytes = read_section(&mut file, symtab, length, "symbol table")?;
    let mut symbols = Vec::new();
    for index in 0..count {
        let start = index.checked_mul(24).ok_or("symbol offset overflow")?;
        let entry = symbol_bytes
            .get(start..start.saturating_add(24))
            .ok_or("symbol slice overflow")?;
        let kind = *entry.get(4).ok_or("truncated symbol")? & 0x0f;
        let section = le16(entry, 6)?;
        if section == 0 || !matches!(kind, 0 | 2) {
            continue;
        }
        let name_at = usize::try_from(le32(entry, 0)?).map_err(|_| "symbol name overflow")?;
        let name = c_string(&strings, name_at)?;
        if name.is_empty() {
            continue;
        }
        symbols.push(Symbol {
            value: le64(entry, 8)?,
            size: le64(entry, 16)?,
            name_at,
        });
    }
    symbols.sort_by(|left, right| {
        left.value
            .cmp(&right.value)
            .then_with(|| left.name_at.cmp(&right.name_at))
    });
    let mut max_end = 0u64;
    let max_ends = symbols
        .iter()
        .map(|symbol| {
            let end = if symbol.size == 0 {
                symbol.value
            } else {
                symbol.value.saturating_add(symbol.size)
            };
            max_end = max_end.max(end);
            max_end
        })
        .collect();
    drop(symbol_bytes);
    let mut loaded = Symbols {
        entries: symbols,
        max_ends,
        strings,
        lines: None,
        line_error: None,
    };
    let line_result = (|| {
        let section = named_section(&sections, &section_names, b".debug_line")?
            .ok_or("debug companion has no .debug_line section")?;
        if section.kind != 1 || section.flags & 0x800 != 0 {
            return Err("debug companion has an unsupported .debug_line section".into());
        }
        if section.size == 0 || section.size > crate::dwarf::MAX_LINE_SECTION_BYTES {
            return Err(format!(
                ".debug_line exceeds {} bytes or is empty",
                crate::dwarf::MAX_LINE_SECTION_BYTES
            ));
        }
        let line_string_section = named_section(&sections, &section_names, b".debug_line_str")?;
        let debug_string_section = named_section(&sections, &section_names, b".debug_str")?;
        if line_string_section.is_some_and(|candidate| {
            candidate.kind != 1
                || candidate.flags & 0x800 != 0
                || candidate.size > crate::dwarf::MAX_LINE_STRING_BYTES
        }) {
            return Err("debug companion has an unsupported .debug_line_str section".into());
        }
        let fixed_loader_bytes = section_names
            .capacity()
            .checked_add(
                sections
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Section>()),
            )
            .ok_or("debug loader byte count overflow")?;
        let mut required_line_input =
            usize::try_from(section.size).map_err(|_| "DWARF line input byte count overflow")?;
        if let Some(candidate) = line_string_section {
            required_line_input = required_line_input
                .checked_add(
                    usize::try_from(candidate.size)
                        .map_err(|_| "DWARF line input byte count overflow")?,
                )
                .ok_or("DWARF line input byte count overflow")?;
        }
        let required_before_parse = loaded
            .heap_bytes()
            .checked_add(fixed_loader_bytes)
            .and_then(|value| value.checked_add(required_line_input))
            .ok_or("debug loader byte count overflow")?;
        if required_before_parse >= MAX_OBJECT_DEBUG_BYTES {
            return Err(format!(
                "debug data expands beyond {MAX_OBJECT_DEBUG_BYTES} loader bytes"
            ));
        }
        let line_bytes = read_section(&mut file, section, length, ".debug_line")?;
        let line_strings = line_string_section
            .map(|section| read_section(&mut file, section, length, ".debug_line_str"))
            .transpose()?
            .unwrap_or_default();
        let parse_lines = |debug_strings: &[u8]| {
            let input_bytes = fixed_loader_bytes
                .checked_add(line_bytes.capacity())
                .and_then(|value| value.checked_add(line_strings.capacity()))
                .and_then(|value| value.checked_add(debug_strings.len()))
                .ok_or("DWARF line input byte count overflow")?;
            let available = MAX_OBJECT_DEBUG_BYTES
                .checked_sub(loaded.heap_bytes())
                .and_then(|value| value.checked_sub(input_bytes))
                .ok_or_else(|| {
                    format!("debug data expands beyond {MAX_OBJECT_DEBUG_BYTES} loader bytes")
                })?;
            crate::dwarf::parse(&line_bytes, &line_strings, debug_strings, available)
        };
        match parse_lines(&[]) {
            Err(error) if error == crate::dwarf::DEBUG_STR_REQUIRED => {
                let candidate = debug_string_section
                    .ok_or("DWARF line table requires an absent .debug_str section")?;
                let committed = loaded
                    .heap_bytes()
                    .checked_add(fixed_loader_bytes)
                    .and_then(|value| value.checked_add(line_bytes.capacity()))
                    .and_then(|value| value.checked_add(line_strings.capacity()))
                    .ok_or("debug loader byte count overflow")?;
                validate_debug_string_section(candidate, committed)?;
                let debug_strings = read_section(&mut file, candidate, length, ".debug_str")?;
                if debug_strings.is_empty() {
                    return Err("DWARF line table requires a nonempty .debug_str section".into());
                }
                parse_lines(&debug_strings)
            }
            result => result,
        }
    })();
    match line_result {
        Ok(lines) => loaded.lines = Some(lines),
        Err(error) => loaded.line_error = Some(error),
    }
    if loaded.heap_bytes() > MAX_OBJECT_DEBUG_BYTES {
        return Err(format!(
            "parsed debug data expands beyond {MAX_OBJECT_DEBUG_BYTES} bytes"
        ));
    }
    Ok(loaded)
}

struct Section {
    name_at: u32,
    kind: u32,
    flags: u64,
    offset: u64,
    size: u64,
    link: u32,
    entsize: u64,
}

fn validate_debug_string_section(candidate: &Section, committed: usize) -> Result<(), String> {
    if candidate.kind != 1
        || candidate.flags & 0x800 != 0
        || candidate.size > crate::dwarf::MAX_LINE_STRING_BYTES
    {
        return Err("debug companion has an unsupported .debug_str section".into());
    }
    let size =
        usize::try_from(candidate.size).map_err(|_| "DWARF debug-string byte count overflow")?;
    if committed
        .checked_add(size)
        .is_none_or(|total| total >= MAX_OBJECT_DEBUG_BYTES)
    {
        return Err(format!(
            ".debug_str would exceed {MAX_OBJECT_DEBUG_BYTES} loader bytes"
        ));
    }
    Ok(())
}

fn named_section<'a>(
    sections: &'a [Section],
    names: &[u8],
    wanted: &[u8],
) -> Result<Option<&'a Section>, String> {
    let mut found = None;
    for section in sections {
        let name_at =
            usize::try_from(section.name_at).map_err(|_| "section name offset overflow")?;
        if c_string(names, name_at)? != wanted {
            continue;
        }
        if found.is_some() {
            return Err(format!(
                "debug companion has duplicate {} sections",
                String::from_utf8_lossy(wanted)
            ));
        }
        found = Some(section);
    }
    Ok(found)
}

fn read_section(
    file: &mut File,
    section: &Section,
    file_length: u64,
    label: &str,
) -> Result<Vec<u8>, String> {
    let end = section
        .offset
        .checked_add(section.size)
        .ok_or_else(|| format!("{label} range overflow"))?;
    if end > file_length {
        return Err(format!("{label} runs past the debug companion"));
    }
    read_at(
        file,
        section.offset,
        usize::try_from(section.size).map_err(|_| format!("{label} does not fit memory"))?,
    )
}

fn c_string(bytes: &[u8], at: usize) -> Result<&[u8], String> {
    let rest = bytes
        .get(at..)
        .ok_or("ELF string offset is out of bounds")?;
    let end = rest
        .iter()
        .position(|byte| *byte == 0)
        .ok_or("unterminated ELF string")?;
    rest.get(..end)
        .ok_or_else(|| "ELF string slice overflow".into())
}

fn require_elf64(header: &[u8]) -> Result<(), String> {
    if header.get(..7) != Some(b"\x7fELF\x02\x01\x01") || le16(header, 18)? != 62 {
        return Err("object is not little-endian ELF64 x86-64".into());
    }
    Ok(())
}

fn read_at(file: &mut File, offset: u64, length: usize) -> Result<Vec<u8>, String> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| e.to_string())?;
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes)
}

fn le16(bytes: &[u8], at: usize) -> Result<u16, String> {
    let value: [u8; 2] = bytes
        .get(at..at.saturating_add(2))
        .ok_or("truncated ELF u16")?
        .try_into()
        .map_err(|_| "truncated ELF u16")?;
    Ok(u16::from_le_bytes(value))
}

fn le32(bytes: &[u8], at: usize) -> Result<u32, String> {
    let value: [u8; 4] = bytes
        .get(at..at.saturating_add(4))
        .ok_or("truncated ELF u32")?
        .try_into()
        .map_err(|_| "truncated ELF u32")?;
    Ok(u32::from_le_bytes(value))
}

fn le64(bytes: &[u8], at: usize) -> Result<u64, String> {
    let value: [u8; 8] = bytes
        .get(at..at.saturating_add(8))
        .ok_or("truncated ELF u64")?
        .try_into()
        .map_err(|_| "truncated ELF u64")?;
    Ok(u64::from_le_bytes(value))
}

pub fn resolved_json(resolved: &Resolved) -> String {
    let source = resolved.source.as_ref();
    let source_line = source
        .map(|value| value.line.to_string())
        .unwrap_or_else(|| "null".into());
    let source_column = source
        .map(|value| value.column.to_string())
        .unwrap_or_else(|| "null".into());
    let discriminator = source
        .map(|value| value.discriminator.to_string())
        .unwrap_or_else(|| "null".into());
    format!(
        "{},\"object_address\":{},\"function_address\":{},\"build_id\":\"{}\",\
         \"assembly_boundary\":{},\"line_resolved\":{},{},\"source_line\":{},\
         \"source_column\":{},\"discriminator\":{},{},{},{}",
        json::named_bytes("function", &resolved.function),
        resolved.object_address,
        resolved.function_address,
        json::hex(&resolved.build_id),
        resolved.assembly_boundary,
        source.is_some(),
        json::named_bytes(
            "source_file",
            source.map_or(&[][..], |value| value.file.as_slice())
        ),
        source_line,
        source_column,
        discriminator,
        json::named_bytes("object", &resolved.object),
        json::named_bytes("debug", &resolved.debug),
        json::named_bytes("provenance", &resolved.provenance)
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{
        has_assembly_boundary, load_build_id, load_index, load_symbols, store_path,
        validate_debug_string_section, Object, Section, Symbol, Symbolizer, Symbols,
    };
    use crate::state::Frame;
    use std::collections::BTreeMap;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    #[test]
    fn store_paths_are_absolute_normal_descendants() {
        assert!(store_path(Path::new("/td/store/hash-name/bin/program")));
        assert!(!store_path(Path::new("/td/store")));
        assert!(!store_path(Path::new("td/store/hash-name")));
        assert!(!store_path(Path::new("/td/store/../outside")));
        assert!(!store_path(Path::new("/td/storex/hash-name")));
    }

    #[test]
    fn unavailable_inode_generation_is_a_same_inode_fallback() {
        let mut objects = BTreeMap::new();
        objects.insert(
            (8, 2, 42, 0),
            Object {
                runtime: "/td/store/x/bin/a".into(),
                debug: "/td/store/x/lib/debug/bin/a.debug".into(),
                build_id: vec![0; 20],
                provenance: b"store-item=x".to_vec(),
                segments: vec![],
                symbols: Some(Err("not needed by this identity test".into())),
                last_used: 0,
                used: false,
                line_error_reported: false,
            },
        );
        let mut symbolizer = Symbolizer {
            objects,
            errors: vec![],
            omitted_errors: 0,
            persistent_error_count: 0,
            persistent_omitted_errors: 0,
            use_clock: 0,
        };
        assert!(symbolizer
            .resolve(
                &Frame {
                    address: 1,
                    relative: Some(1),
                    major: 8,
                    minor: 2,
                    inode: 42,
                    inode_generation: 9,
                    path: vec![],
                },
                4096,
            )
            .is_none());
        assert!(symbolizer.errors.is_empty());
        assert!(has_assembly_boundary(b"store-item=x;assembly-boundary=1"));
        symbolizer.begin_report();
        assert!(symbolizer
            .objects
            .values()
            .all(|object| object.symbols.is_none()));
    }

    #[test]
    fn cache_retains_multiple_tables_below_its_combined_ceiling() {
        let symbols = || Symbols {
            entries: vec![],
            max_ends: vec![],
            strings: vec![0; 1024],
            lines: None,
            line_error: None,
        };
        let object = |last_used| Object {
            runtime: "/td/store/x/bin/a".into(),
            debug: "/td/store/x/lib/debug/bin/a.debug".into(),
            build_id: vec![0; 20],
            provenance: b"store-item=x".to_vec(),
            segments: vec![],
            symbols: Some(Ok(symbols())),
            last_used,
            used: false,
            line_error_reported: false,
        };
        let keep = (8, 2, 44, 0);
        let mut symbolizer = Symbolizer::default();
        symbolizer.objects.insert((8, 2, 42, 0), object(1));
        symbolizer.objects.insert((8, 2, 43, 0), object(2));
        symbolizer.objects.insert(keep, object(3));
        symbolizer.evict_symbol_tables(&keep, 1024);
        assert_eq!(
            symbolizer
                .objects
                .values()
                .filter(|object| matches!(object.symbols.as_ref(), Some(Ok(_))))
                .count(),
            3
        );
    }

    #[test]
    fn unusable_debug_string_sections_have_a_direct_diagnostic() {
        let section = |flags, size| Section {
            name_at: 0,
            kind: 1,
            flags,
            offset: 0,
            size,
            link: 0,
            entsize: 0,
        };
        assert!(validate_debug_string_section(&section(0x800, 1), 0)
            .unwrap_err()
            .contains("unsupported .debug_str"));
        assert!(validate_debug_string_section(
            &section(0, crate::dwarf::MAX_LINE_STRING_BYTES + 1),
            0,
        )
        .unwrap_err()
        .contains("unsupported .debug_str"));
        assert!(
            validate_debug_string_section(&section(0, 1), super::MAX_OBJECT_DEBUG_BYTES - 1,)
                .unwrap_err()
                .contains("would exceed")
        );
    }

    #[test]
    fn symbol_holes_remain_unresolved_and_used_identities_are_per_report() {
        let mut objects = BTreeMap::new();
        objects.insert(
            (8, 2, 42, 0),
            Object {
                runtime: "/td/store/x/bin/a".into(),
                debug: "/td/store/x/lib/debug/bin/a.debug".into(),
                build_id: vec![1; 20],
                provenance: b"store-item=x".to_vec(),
                segments: vec![],
                symbols: Some(Ok(Symbols {
                    entries: vec![
                        Symbol {
                            value: 0x100,
                            size: 0x10,
                            name_at: 0,
                        },
                        Symbol {
                            value: 0x105,
                            size: 0,
                            name_at: 6,
                        },
                        Symbol {
                            value: 0x300,
                            size: 0,
                            name_at: 12,
                        },
                    ],
                    max_ends: vec![0x110, 0x110, 0x300],
                    strings: b"known\0alias\0point\0".to_vec(),
                    lines: None,
                    line_error: None,
                })),
                last_used: 0,
                used: false,
                line_error_reported: false,
            },
        );
        let mut symbolizer = Symbolizer {
            objects,
            errors: vec![],
            omitted_errors: 0,
            persistent_error_count: 0,
            persistent_omitted_errors: 0,
            use_clock: 0,
        };
        let frame = |relative| Frame {
            address: relative,
            relative: Some(relative),
            major: 8,
            minor: 2,
            inode: 42,
            inode_generation: 0,
            path: vec![],
        };
        assert!(symbolizer.resolve(&frame(0x200), 4096).is_none());
        assert!(symbolizer
            .errors
            .iter()
            .any(|error| error.contains("no function symbol")));
        assert!(symbolizer.resolve(&frame(0x105), 4096).is_some());
        assert!(symbolizer.resolve(&frame(0x106), 4096).is_some());
        assert!(symbolizer.resolve(&frame(0x300), 4096).is_some());
        assert!(symbolizer.resolve(&frame(0x301), 4096).is_none());
        assert!(symbolizer
            .identities_json(4096)
            .unwrap()
            .contains("/td/store/x/bin/a"));

        symbolizer.begin_report();
        assert!(symbolizer.errors.is_empty());
        assert_eq!(symbolizer.identities_json(4096).unwrap(), "");
    }

    #[test]
    fn object_index_itself_must_not_be_a_symlink() {
        let base = std::env::temp_dir().join(format!(
            "td-profiler-object-index-test-{}",
            std::process::id()
        ));
        let target = base.with_extension("target");
        let link = base.with_extension("link");
        std::fs::write(&target, b"td-profiler-objects-v1\n").unwrap();
        symlink(&target, &link).unwrap();
        let error = load_index(&link).err().unwrap_or_default();
        assert!(error.contains("not a regular file"));
        std::fs::remove_file(link).unwrap();
        std::fs::remove_file(target).unwrap();
    }

    #[test]
    fn build_id_reader_requires_one_sha1_note() {
        let mut elf = vec![0u8; 228];
        elf.get_mut(..7)
            .unwrap()
            .copy_from_slice(b"\x7fELF\x02\x01\x01");
        elf.get_mut(18..20)
            .unwrap()
            .copy_from_slice(&62u16.to_le_bytes());
        elf.get_mut(40..48)
            .unwrap()
            .copy_from_slice(&64u64.to_le_bytes());
        elf.get_mut(58..60)
            .unwrap()
            .copy_from_slice(&64u16.to_le_bytes());
        elf.get_mut(60..62)
            .unwrap()
            .copy_from_slice(&2u16.to_le_bytes());
        elf.get_mut(132..136)
            .unwrap()
            .copy_from_slice(&7u32.to_le_bytes());
        elf.get_mut(152..160)
            .unwrap()
            .copy_from_slice(&192u64.to_le_bytes());
        elf.get_mut(160..168)
            .unwrap()
            .copy_from_slice(&36u64.to_le_bytes());
        elf.get_mut(192..196)
            .unwrap()
            .copy_from_slice(&4u32.to_le_bytes());
        elf.get_mut(196..200)
            .unwrap()
            .copy_from_slice(&20u32.to_le_bytes());
        elf.get_mut(200..204)
            .unwrap()
            .copy_from_slice(&3u32.to_le_bytes());
        elf.get_mut(204..208).unwrap().copy_from_slice(b"GNU\0");
        elf.get_mut(208..228).unwrap().copy_from_slice(&[0x5a; 20]);

        let path =
            std::env::temp_dir().join(format!("td-profiler-build-id-test-{}", std::process::id()));
        std::fs::write(&path, &elf).unwrap();
        assert_eq!(load_build_id(&path).unwrap(), vec![0x5a; 20]);
        std::fs::remove_file(path).unwrap();
    }

    fn line_unit_v4() -> Vec<u8> {
        let mut header = vec![1, 1, 1, 0xfb, 14, 13];
        header.extend_from_slice(&[0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1]);
        header.extend_from_slice(b"src\0\0main.rs\0\x01\0\0\0");
        let mut program = vec![0, 9, 2];
        program.extend_from_slice(&0x1000u64.to_le_bytes());
        program.extend_from_slice(&[3, 9, 1, 2, 4, 0, 1, 1]);
        let length = 2usize + 4 + header.len() + program.len();
        let mut unit = Vec::new();
        unit.extend_from_slice(&(u32::try_from(length).unwrap()).to_le_bytes());
        unit.extend_from_slice(&4u16.to_le_bytes());
        unit.extend_from_slice(&(u32::try_from(header.len()).unwrap()).to_le_bytes());
        unit.extend_from_slice(&header);
        unit.extend_from_slice(&program);
        unit
    }

    #[test]
    fn elf_loader_joins_symbols_to_uncompressed_line_sections() {
        const SECTION_COUNT: usize = 6;
        let mut elf = vec![0u8; 64 + SECTION_COUNT * 64];
        elf.get_mut(..7)
            .unwrap()
            .copy_from_slice(b"\x7fELF\x02\x01\x01");
        elf.get_mut(18..20)
            .unwrap()
            .copy_from_slice(&62u16.to_le_bytes());
        elf.get_mut(40..48)
            .unwrap()
            .copy_from_slice(&64u64.to_le_bytes());
        elf.get_mut(58..60)
            .unwrap()
            .copy_from_slice(&64u16.to_le_bytes());
        elf.get_mut(60..62)
            .unwrap()
            .copy_from_slice(&(SECTION_COUNT as u16).to_le_bytes());
        elf.get_mut(62..64)
            .unwrap()
            .copy_from_slice(&1u16.to_le_bytes());

        let names = b"\0.shstrtab\0.symtab\0.strtab\0.debug_line\0.text\0";
        let names_offset = elf.len();
        elf.extend_from_slice(names);
        let symbols_offset = elf.len();
        let mut symbols = vec![0u8; 48];
        symbols
            .get_mut(24..28)
            .unwrap()
            .copy_from_slice(&1u32.to_le_bytes());
        *symbols.get_mut(28).unwrap() = 2;
        symbols
            .get_mut(30..32)
            .unwrap()
            .copy_from_slice(&5u16.to_le_bytes());
        symbols
            .get_mut(32..40)
            .unwrap()
            .copy_from_slice(&0x1000u64.to_le_bytes());
        symbols
            .get_mut(40..48)
            .unwrap()
            .copy_from_slice(&0x10u64.to_le_bytes());
        elf.extend_from_slice(&symbols);
        let strings_offset = elf.len();
        elf.extend_from_slice(b"\0f\0");
        let lines = line_unit_v4();
        let lines_offset = elf.len();
        elf.extend_from_slice(&lines);

        let section = |elf: &mut [u8],
                       index: usize,
                       name: u32,
                       kind: u32,
                       offset: usize,
                       size: usize,
                       link: u32,
                       entsize: u64| {
            let at = 64 + index * 64;
            elf.get_mut(at..at + 4)
                .unwrap()
                .copy_from_slice(&name.to_le_bytes());
            elf.get_mut(at + 4..at + 8)
                .unwrap()
                .copy_from_slice(&kind.to_le_bytes());
            elf.get_mut(at + 24..at + 32)
                .unwrap()
                .copy_from_slice(&(offset as u64).to_le_bytes());
            elf.get_mut(at + 32..at + 40)
                .unwrap()
                .copy_from_slice(&(size as u64).to_le_bytes());
            elf.get_mut(at + 40..at + 44)
                .unwrap()
                .copy_from_slice(&link.to_le_bytes());
            elf.get_mut(at + 56..at + 64)
                .unwrap()
                .copy_from_slice(&entsize.to_le_bytes());
        };
        section(&mut elf, 1, 1, 3, names_offset, names.len(), 0, 0);
        section(&mut elf, 2, 11, 2, symbols_offset, 48, 3, 24);
        section(&mut elf, 3, 19, 3, strings_offset, 3, 0, 0);
        section(&mut elf, 4, 27, 1, lines_offset, lines.len(), 0, 0);
        section(&mut elf, 5, 39, 1, 0, 0, 0, 0);

        let path = std::env::temp_dir().join(format!(
            "td-profiler-debug-lines-test-{}",
            std::process::id()
        ));
        std::fs::write(&path, elf).unwrap();
        let loaded = load_symbols(&path).unwrap();
        assert!(loaded.line_error.is_none(), "{:?}", loaded.line_error);
        let lines = loaded.lines.unwrap();
        let location = lines.resolve(0x1001).unwrap();
        assert_eq!(location.file, b"src/main.rs");
        assert_eq!(location.line, 10);
        std::fs::remove_file(path).unwrap();
    }
}
