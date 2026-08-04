//! td-term's terminfo entry.
//!
//! A human-readable capability source, the compiler that turns it into the
//! legacy binary format, and the decoder the tests read the result back with.
//! `tic`, ncurses, and a host terminfo database are not build inputs; the
//! shipped multicall emits its own entry.
//!
//! Position in the three name tables below IS the wire index, so those tables
//! are the whole trust surface: a capability written at the wrong index is a
//! well-formed entry that means something else, and no round-trip through this
//! module's own decoder can see it. They are pinned as ordered lists rather
//! than as per-capability integers so the thing a reviewer checks is one list
//! against ncurses' `Caps`, not a scatter of literals.

/// Boolean capabilities in ncurses `Caps` order.
const BOOLEANS: &[&str] = &[
    "bw", "am", "xsb", "xhp", "xenl", "eo", "gn", "hc", "km", "hs", "in", "da", "db", "mir", "msgr",
    "os", "eslok", "xt", "hz", "ul", "xon", "nxon", "mc5i", "chts", "nrrmc", "npc", "ndscr", "ccc",
    "bce", "hls", "xhpa", "crxm", "daisy", "xvpa", "sam", "cpix", "lpix", "OTbs", "OTns", "OTnc",
    "OTMT", "OTNL", "OTpt", "OTxr",
];

/// Numeric capabilities in ncurses `Caps` order.
const NUMBERS: &[&str] = &[
    "cols", "it", "lines", "lm", "xmc", "pb", "vt", "wsl", "nlab", "lh", "lw", "ma", "wnum",
    "colors", "pairs", "ncv", "bufsz", "spinv", "spinh", "maddr", "mjump", "mcs", "mls", "npins",
    "orc", "orl", "orhi", "orvi", "cps", "widcs", "btns", "bitwin", "bitype", "OTug", "OTdC",
    "OTdN", "OTdB", "OTdT", "OTkn",
];

/// String capabilities in ncurses `Caps` order, truncated one past the highest
/// index td-term claims (`setab`, 360). The entry declares exactly this many
/// offsets and a reader treats every later capability as absent, so the 53
/// names beyond it — printer, bit-image and obsolete entries this profile has
/// no use for — are deliberately not pinned rather than pinned from memory.
///
/// `lf10` sits between `lf1` and `lf2`, the same alphabetical-as-a-string
/// quirk `kf10` has between `kf1` and `kf2`; the label family runs `lf0`
/// through `lf10`, not `lf0` through `lf9`.
const STRINGS: &[&str] = &[
    "cbt", "bel", "cr", "csr", "tbc", "clear", "el", "ed", "hpa", "cmdch", "cup", "cud1", "home",
    "civis", "cub1", "mrcup", "cnorm", "cuf1", "ll", "cuu1", "cvvis", "dch1", "dl1", "dsl", "hd",
    "smacs", "blink", "bold", "smcup", "smdc", "dim", "smir", "invis", "prot", "rev", "smso",
    "smul", "ech", "rmacs", "sgr0", "rmcup", "rmdc", "rmir", "rmso", "rmul", "flash", "ff", "fsl",
    "is1", "is2", "is3", "if", "ich1", "il1", "ip", "kbs", "ktbc", "kclr", "kctab", "kdch1", "kdl1",
    "kcud1", "krmir", "kel", "ked", "kf0", "kf1", "kf10", "kf2", "kf3", "kf4", "kf5", "kf6", "kf7",
    "kf8", "kf9", "khome", "kich1", "kil1", "kcub1", "kll", "knp", "kpp", "kcuf1", "kind", "kri",
    "khts", "kcuu1", "rmkx", "smkx", "lf0", "lf1", "lf10", "lf2", "lf3", "lf4", "lf5", "lf6",
    "lf7", "lf8", "lf9", "rmm", "smm", "nel", "pad", "dch", "dl", "cud", "ich", "indn", "il",
    "cub", "cuf", "rin",
    "cuu", "pfkey", "pfloc", "pfx", "mc0", "mc4", "mc5", "rep", "rs1", "rs2", "rs3", "rf", "rc",
    "vpa", "sc", "ind", "ri", "sgr", "hts", "wind", "ht", "tsl", "uc", "hu", "iprog", "ka1", "ka3",
    "kb2", "kc1", "kc3", "mc5p", "rmp", "acsc", "pln", "kcbt", "smxon", "rmxon", "smam", "rmam",
    "xonc", "xoffc", "enacs", "smln", "rmln", "kbeg", "kcan", "kclo", "kcmd", "kcpy", "kcrt",
    "kend", "kent", "kext", "kfnd", "khlp", "kmrk", "kmsg", "kmov", "knxt", "kopn", "kopt", "kprv",
    "kprt", "krdo", "kref", "krfr", "krpl", "krst", "kres", "ksav", "kspd", "kund", "kBEG", "kCAN",
    "kCMD", "kCPY", "kCRT", "kDC", "kDL", "kslt", "kEND", "kEOL", "kEXT", "kFND", "kHLP", "kHOM",
    "kIC", "kLFT", "kMSG", "kMOV", "kNXT", "kOPT", "kPRV", "kPRT", "kRDO", "kRPL", "kRIT", "kRES",
    "kSAV", "kSPD", "kUND", "rfi", "kf11", "kf12", "kf13", "kf14", "kf15", "kf16", "kf17", "kf18",
    "kf19", "kf20", "kf21", "kf22", "kf23", "kf24", "kf25", "kf26", "kf27", "kf28", "kf29", "kf30",
    "kf31", "kf32", "kf33", "kf34", "kf35", "kf36", "kf37", "kf38", "kf39", "kf40", "kf41", "kf42",
    "kf43", "kf44", "kf45", "kf46", "kf47", "kf48", "kf49", "kf50", "kf51", "kf52", "kf53", "kf54",
    "kf55", "kf56", "kf57", "kf58", "kf59", "kf60", "kf61", "kf62", "kf63", "el1", "mgc", "smgl",
    "smgr", "fln", "sclk", "dclk", "rmclk", "cwin", "wingo", "hup", "dial", "qdial", "tone",
    "pulse", "hook", "pause", "wait", "u0", "u1", "u2", "u3", "u4", "u5", "u6", "u7", "u8", "u9",
    "op", "oc", "initc", "initp", "scp", "setf", "setb", "cpi", "lpi", "chr", "cvr", "defc",
    "swidm", "sdrfq", "sitm", "slm", "smicm", "snlq", "snrmq", "sshm", "ssubm", "ssupm", "sum",
    "rwidm", "ritm", "rlm", "rmicm", "rshm", "rsubm", "rsupm", "rum", "mhpa", "mcud1", "mcub1",
    "mcuf1", "mvpa", "mcuu1", "porder", "mcud", "mcub", "mcuf", "mcuu", "scs", "smgb", "smgbp",
    "smglp", "smgrp", "smgt", "smgtp", "sbim", "scsd", "rbim", "rcsd", "subcs", "supcs", "docr",
    "zerom", "csnm", "kmous", "minfo", "reqmp", "getm", "setaf", "setab",
];

/// The capability source. Each line is `bool NAME CASE`, `num NAME VALUE CASE`
/// or `str NAME VALUE CASE`, where CASE names the blocking native corpus case
/// that exercises the capability — nothing is claimed here that the corpus
/// does not already prove about the model or the keyboard adapter.
///
/// Values use terminfo's own escapes (`\E`, `^G`, `\NNN`) so they can be read
/// against any other entry.
const SOURCE: &str = r#"
name td-term|td native terminal

# Autowrap with the deferred-wrap (`xenl`) column, and erase that keeps the
# background colour but no other rendition.
bool am    wrapping/right-margin
bool xenl  wrapping/right-margin
bool bce   color/erase-retains-only-colors

# `cols`/`lines` are deliberately absent: td-term sets and verifies the PTY
# winsize before the child starts, so the pre-winsize fallback they exist to
# serve is unreachable by construction.
num it     8      cursor/tabs-save-and-restore
num colors 256    color/indexed-and-rgb
num pairs  32767  color/indexed-and-rgb

# Controls and single-cell motion. `bel` is deliberately absent: BEL sets the
# model's coalesced visual-bell bit, and the corpus has no observation for it
# until the renderer that presents it lands.
str cr    ^M      parser/controls
str ht    ^I      cursor/tabs-save-and-restore
str ind   \ED     libvterm/11state-movecursor/index
str ri    \EM     libvterm/11state-movecursor/newline
str nel   \EE     libvterm/11state-movecursor/newline
str hts   \EH     libvterm/21state-tabstops/hts
str tbc   \E[3g   libvterm/21state-tabstops/tbc-3
str cbt   \E[Z    libvterm/11state-movecursor/cursor-backward-tab
str sc    \E7     cursor/tabs-save-and-restore
str rc    \E8     cursor/tabs-save-and-restore
str rs1   \Ec     cursor/restored-origin-cursor-moves-outside-margins

# Cursor addressing.
str cup   \E[%i%p1%d;%p2%dH   cursor/absolute-and-relative
str home  \E[H                cursor/absolute-and-relative
str hpa   \E[%i%p1%dG         libvterm/11state-movecursor/bounds-checking
str vpa   \E[%i%p1%dd         libvterm/11state-movecursor/vertical-position-absolute
str cuu1  \E[A                cursor/absolute-and-relative
str cud1  \E[B                cursor/absolute-and-relative
str cuf1  \E[C                cursor/absolute-and-relative
str cub1  \E[D                cursor/absolute-and-relative
str cuu   \E[%p1%dA           cursor/absolute-and-relative
str cud   \E[%p1%dB           cursor/absolute-and-relative
str cuf   \E[%p1%dC           cursor/absolute-and-relative
str cub   \E[%p1%dD           cursor/absolute-and-relative

# Erase and edit. `rep` emits the character once and repeats it `n-1` more
# times, so `n = 1` sends `CSI 0 b`, which this model (and libvterm, and
# xterm) treats as one repeat rather than none — a direct `tput rep X 1`
# therefore prints two. ncurses only reaches for `rep` on runs long enough to
# beat its padding cost, so this is a documented parity, not a defect.
str clear \E[H\E[2J      libvterm/13state-edit/ed-2
str ed    \E[J           libvterm/13state-edit/ed-0
str el    \E[K           libvterm/60screen-ascii/erase
str el1   \E[1K          libvterm/13state-edit/el-1
str ich1  \E[@           libvterm/13state-edit/ich
str ich   \E[%p1%d@      libvterm/13state-edit/ich
str dch1  \E[P           libvterm/13state-edit/dch
str dch   \E[%p1%dP      libvterm/13state-edit/dch
str ech   \E[%p1%dX      libvterm/13state-edit/ech
str il1   \E[L           editing/insert-and-delete-lines
str il    \E[%p1%dL      editing/insert-and-delete-lines
str dl1   \E[M           editing/insert-and-delete-lines
str dl    \E[%p1%dM      editing/insert-and-delete-lines
str rep   %p1%c\E[%p2%{1}%-%db  wrapping/repeat-last-scalar

# Scrolling region.
str csr   \E[%i%p1%d;%p2%dr   editing/scroll-region
str indn  \E[%p1%dS           libvterm/12state-scroll/decstbm-resets-cursor-position
str rin   \E[%p1%dT           libvterm/12state-scroll/decstbm-resets-cursor-position

# Rendition. Blink, conceal and strike-through have no SGR in this profile's
# model, so no capability claims them.
str sgr0  \E[m     libvterm/64screen-pen/background
str bold  \E[1m    color/rendition-set-and-reset
str dim   \E[2m    color/rendition-set-and-reset
str sitm  \E[3m    color/rendition-set-and-reset
str ritm  \E[23m   color/rendition-set-and-reset
str smul  \E[4m    color/rendition-set-and-reset
str rmul  \E[24m   color/rendition-set-and-reset
str rev   \E[7m    color/rendition-set-and-reset
str smso  \E[7m    color/rendition-set-and-reset
str rmso  \E[27m   color/rendition-set-and-reset
str op    \E[39;49m  color/bright-and-reset
str setaf \E[%?%p1%{8}%<%t3%p1%d%e%p1%{16}%<%t9%p1%{8}%-%d%e38;5;%p1%d%;m  color/indexed-and-rgb
str setab \E[%?%p1%{8}%<%t4%p1%d%e%p1%{16}%<%t10%p1%{8}%-%d%e48;5;%p1%d%;m  color/indexed-palette-background

# Private modes.
str smcup \E[?1049h  libvterm/60screen-ascii/altscreen
str rmcup \E[?1049l  libvterm/60screen-ascii/altscreen
str civis \E[?25l    libvterm/18state-termprops/cursor-visibility
str cnorm \E[?25h    libvterm/18state-termprops/cursor-visibility
str smkx  \E[?1h     modes/cursor-and-application-state
str rmkx  \E[?1l     modes/cursor-and-application-state
str smam  \E[?7h     libvterm/20state-wrapping/80th-column-causes-linefeed-on-wraparound
str rmam  \E[?7l     libvterm/20state-wrapping/80th-column-causes-linefeed-on-wraparound

# DEC special graphics, and the line-drawing map the model implements.
str smacs \E(0  cursor/dec-save-restores-rendition-and-charset
str rmacs \E(B  cursor/dec-save-restores-rendition-and-charset
str acsc  ``aabbccddeeffgghhiijjkkllmmnnooppqqrrssttuuvvwwxxyyzz{{||}}~~  parser/dec-special-graphics

# Replies. u7 asks, u6 is the shape of the answer; u9 asks, u8 is the answer.
str u6  \E[%i%d;%dR  replies/status-and-position
str u7  \E[6n        replies/status-and-position
str u8  \E[?1;0c     replies/device-attributes
str u9  \E[c         replies/device-attributes

# Keys, in the application forms the terminal sends once `smkx` is emitted.
str kbs    \177     input/named-keys-use-terminal-bytes
str kcbt   \E[Z     input/named-keys-use-terminal-bytes
str kcuu1  \EOA     input/cursor-keys-follow-application-mode
str kcud1  \EOB     input/cursor-keys-follow-application-mode
str kcuf1  \EOC     input/cursor-keys-follow-application-mode
str kcub1  \EOD     input/cursor-keys-follow-application-mode
str khome  \EOH     input/cursor-keys-follow-application-mode
str kend   \EOF     input/cursor-keys-follow-application-mode
str kpp    \E[5~    input/paging-keys-are-mode-independent
str knp    \E[6~    input/paging-keys-are-mode-independent
str kich1  \E[2~    input/named-keys-use-terminal-bytes
str kdch1  \E[3~    input/named-keys-use-terminal-bytes
str kf1    \EOP     input/function-keys
str kf2    \EOQ     input/function-keys
str kf3    \EOR     input/function-keys
str kf4    \EOS     input/function-keys
str kf5    \E[15~   input/function-keys
str kf6    \E[17~   input/function-keys
str kf7    \E[18~   input/function-keys
str kf8    \E[19~   input/function-keys
str kf9    \E[20~   input/function-keys
str kf10   \E[21~   input/function-keys
str kf11   \E[23~   input/function-keys
str kf12   \E[24~   input/function-keys
"#;

/// The legacy binary format. The 32-bit-number format (`0o1036`) exists to
/// carry `pairs#65536`; this entry's largest number fits in the signed 16-bit
/// field, so the older format every reader understands is enough.
const MAGIC: u16 = 0o432;
const ABSENT: i16 = -1;
/// A terminfo string capability is NUL-terminated in the table, and offsets are
/// signed 16-bit, so the whole table has to stay addressable by one.
const MAX_TABLE: usize = i16::MAX as usize;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Value {
    Flag,
    Number(i16),
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Capability {
    pub(crate) name: &'static str,
    pub(crate) index: usize,
    pub(crate) value: Value,
    pub(crate) case: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entry {
    pub(crate) names: &'static str,
    pub(crate) capabilities: Vec<Capability>,
}

/// The decoded form of a compiled entry: dense arrays exactly as the file
/// declares them, so a test can see an index the source never mentioned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Decoded {
    pub(crate) names: String,
    pub(crate) booleans: Vec<bool>,
    pub(crate) numbers: Vec<i16>,
    pub(crate) strings: Vec<Option<Vec<u8>>>,
}

fn index_of(table: &[&str], name: &str) -> Option<usize> {
    table.iter().position(|entry| *entry == name)
}

/// Terminfo source escapes. `\NNN` is octal, `^X` is a control byte.
fn unescape(input: &str) -> Result<Vec<u8>, String> {
    let mut output = Vec::with_capacity(input.len());
    let mut bytes = input.bytes();
    while let Some(byte) = bytes.next() {
        match byte {
            b'\\' => {
                let next = bytes
                    .next()
                    .ok_or_else(|| format!("trailing backslash in {input:?}"))?;
                match next {
                    b'E' | b'e' => output.push(0x1b),
                    b'n' => output.push(b'\n'),
                    b'l' => output.push(b'\n'),
                    b'r' => output.push(b'\r'),
                    b't' => output.push(b'\t'),
                    b'b' => output.push(0x08),
                    b'f' => output.push(0x0c),
                    b's' => output.push(b' '),
                    b'^' => output.push(b'^'),
                    b'\\' => output.push(b'\\'),
                    b',' => output.push(b','),
                    b':' => output.push(b':'),
                    b'0'..=b'7' => {
                        // Up to three octal digits, the first already taken.
                        let mut value = u32::from(next - b'0');
                        let mut taken = 1;
                        let mut rest = bytes.clone();
                        while taken < 3 {
                            match rest.next() {
                                Some(digit @ b'0'..=b'7') => {
                                    value = value * 8 + u32::from(digit - b'0');
                                    bytes.next();
                                    taken += 1;
                                }
                                _ => break,
                            }
                            rest = bytes.clone();
                        }
                        let byte = u8::try_from(value)
                            .map_err(|_| format!("octal escape out of range in {input:?}"))?;
                        output.push(byte);
                    }
                    other => {
                        return Err(format!("unknown escape '\\{}' in {input:?}", other as char))
                    }
                }
            }
            b'^' => {
                let next = bytes
                    .next()
                    .ok_or_else(|| format!("trailing caret in {input:?}"))?;
                match next {
                    b'?' => output.push(0x7f),
                    b'@'..=b'_' => output.push(next & 0x1f),
                    other => {
                        return Err(format!("unknown control '^{}' in {input:?}", other as char))
                    }
                }
            }
            other => output.push(other),
        }
    }
    Ok(output)
}

/// Parse the pinned capability source. Every capability must name a table
/// entry, so a typo is a build failure rather than a silently dropped
/// capability.
pub(crate) fn parse() -> Result<Entry, String> {
    let mut names = None;
    let mut capabilities = Vec::new();
    for line in SOURCE.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let kind = fields
            .next()
            .ok_or_else(|| format!("empty capability line {line:?}"))?;
        if kind == "name" {
            let rest = line
                .strip_prefix("name ")
                .ok_or_else(|| format!("malformed name line {line:?}"))?;
            if names.replace(rest.trim()).is_some() {
                return Err("the source declares two names".into());
            }
            continue;
        }
        let name = fields
            .next()
            .ok_or_else(|| format!("capability without a name: {line:?}"))?;
        let (table, value) = match kind {
            "bool" => (BOOLEANS, Value::Flag),
            "num" => {
                let text = fields
                    .next()
                    .ok_or_else(|| format!("number without a value: {line:?}"))?;
                let parsed = text
                    .parse::<i16>()
                    .map_err(|_| format!("number {text:?} is not a signed 16-bit value"))?;
                if parsed < 0 {
                    return Err(format!("negative number in {line:?}"));
                }
                (NUMBERS, Value::Number(parsed))
            }
            "str" => {
                let text = fields
                    .next()
                    .ok_or_else(|| format!("string without a value: {line:?}"))?;
                (STRINGS, Value::Bytes(unescape(text)?))
            }
            other => return Err(format!("unknown capability kind {other:?}")),
        };
        let index =
            index_of(table, name).ok_or_else(|| format!("{name:?} is not a {kind} capability"))?;
        let case = fields
            .next()
            .ok_or_else(|| format!("capability {name:?} names no corpus case"))?;
        if fields.next().is_some() {
            return Err(format!("trailing text on {line:?}"));
        }
        // A capability written twice would silently keep whichever the encoder
        // visited last.
        if capabilities
            .iter()
            .any(|existing: &Capability| existing.name == name)
        {
            return Err(format!("capability {name:?} is declared twice"));
        }
        capabilities.push(Capability {
            name,
            index,
            value,
            case,
        });
    }
    let names = names.ok_or_else(|| "the source declares no name".to_string())?;
    if capabilities.is_empty() {
        return Err("the source declares no capabilities".into());
    }
    Ok(Entry {
        names,
        capabilities,
    })
}

fn push_i16(output: &mut Vec<u8>, value: i16) {
    output.extend_from_slice(&value.to_le_bytes());
}

/// Compile an entry into the legacy binary format.
pub(crate) fn compile(entry: &Entry) -> Result<Vec<u8>, String> {
    let mut booleans = Vec::new();
    let mut numbers = Vec::new();
    let mut strings = Vec::new();
    for capability in &entry.capabilities {
        match &capability.value {
            Value::Flag => booleans.push(capability.index),
            Value::Number(value) => numbers.push((capability.index, *value)),
            Value::Bytes(bytes) => strings.push((capability.index, bytes.clone())),
        }
    }
    // The arrays are declared only as far as the highest capability claimed;
    // a reader treats everything past the declared count as absent.
    let count = |highest: Option<usize>| -> Result<usize, String> {
        match highest {
            None => Ok(0),
            Some(index) => index
                .checked_add(1)
                .ok_or_else(|| "capability index overflow".to_string()),
        }
    };
    let boolean_count = count(booleans.iter().copied().max())?;
    let number_count = count(numbers.iter().map(|(index, _)| *index).max())?;
    let string_count = count(strings.iter().map(|(index, _)| *index).max())?;

    let mut name_bytes = entry.names.as_bytes().to_vec();
    if name_bytes.contains(&0) {
        return Err("the entry name contains a NUL".into());
    }
    name_bytes.push(0);

    let mut table = Vec::new();
    let mut offsets = vec![ABSENT; string_count];
    for (index, bytes) in &strings {
        if bytes.contains(&0) {
            return Err(format!("string capability {index} contains a NUL"));
        }
        let offset = i16::try_from(table.len())
            .map_err(|_| "string table exceeds a 16-bit offset".to_string())?;
        let slot = offsets
            .get_mut(*index)
            .ok_or_else(|| format!("string index {index} is past the declared count"))?;
        *slot = offset;
        table.extend_from_slice(bytes);
        table.push(0);
        if table.len() > MAX_TABLE {
            return Err("string table exceeds a 16-bit offset".into());
        }
    }

    let header = [
        i16::try_from(name_bytes.len()).map_err(|_| "entry name too long".to_string())?,
        i16::try_from(boolean_count).map_err(|_| "too many booleans".to_string())?,
        i16::try_from(number_count).map_err(|_| "too many numbers".to_string())?,
        i16::try_from(string_count).map_err(|_| "too many strings".to_string())?,
        i16::try_from(table.len()).map_err(|_| "string table too long".to_string())?,
    ];

    let mut output = Vec::new();
    output.extend_from_slice(&MAGIC.to_le_bytes());
    for field in header {
        push_i16(&mut output, field);
    }
    output.extend_from_slice(&name_bytes);
    for index in 0..boolean_count {
        output.push(u8::from(booleans.contains(&index)));
    }
    // Numbers are 16-bit and must start on an even boundary; the header is
    // even, so only the names and flags can put it off.
    if (name_bytes.len() + boolean_count) % 2 == 1 {
        output.push(0);
    }
    for index in 0..number_count {
        let mut value = ABSENT;
        for (slot, claimed) in &numbers {
            if *slot == index {
                value = *claimed;
            }
        }
        push_i16(&mut output, value);
    }
    for offset in &offsets {
        push_i16(&mut output, *offset);
    }
    output.extend_from_slice(&table);
    Ok(output)
}

fn read_i16(bytes: &[u8], at: usize) -> Result<i16, String> {
    let end = at.checked_add(2).ok_or_else(|| "offset overflow".to_string())?;
    let slice = bytes
        .get(at..end)
        .ok_or_else(|| format!("entry truncated at {at}"))?;
    let pair: [u8; 2] = slice
        .try_into()
        .map_err(|_| format!("entry truncated at {at}"))?;
    Ok(i16::from_le_bytes(pair))
}

/// Decode a compiled entry. Bounds and counts are checked before any read, so
/// a corrupt file is an error rather than a panic.
pub(crate) fn decode(bytes: &[u8]) -> Result<Decoded, String> {
    let magic = read_i16(bytes, 0)?;
    if magic.to_le_bytes() != MAGIC.to_le_bytes() {
        return Err(format!("magic {magic:#x} is not a terminfo entry"));
    }
    let field = |slot: usize| -> Result<usize, String> {
        let at = slot
            .checked_mul(2)
            .and_then(|scaled| scaled.checked_add(2))
            .ok_or_else(|| "header offset overflow".to_string())?;
        let raw = read_i16(bytes, at)?;
        usize::try_from(raw).map_err(|_| format!("negative header field {slot}"))
    };
    let name_size = field(0)?;
    let boolean_count = field(1)?;
    let number_count = field(2)?;
    let string_count = field(3)?;
    let table_size = field(4)?;

    let mut at: usize = 12;
    let names_end = at
        .checked_add(name_size)
        .ok_or_else(|| "name section overflows".to_string())?;
    let name_bytes = bytes
        .get(at..names_end)
        .ok_or_else(|| "entry truncated in its names".to_string())?;
    let name_body = name_bytes
        .split_last()
        .and_then(|(last, body)| (*last == 0).then_some(body))
        .ok_or_else(|| "the names section is not NUL-terminated".to_string())?;
    let names = std::str::from_utf8(name_body)
        .map_err(|_| "the names section is not UTF-8".to_string())?
        .to_string();
    at = names_end;

    let flags_end = at
        .checked_add(boolean_count)
        .ok_or_else(|| "boolean section overflows".to_string())?;
    let flag_bytes = bytes
        .get(at..flags_end)
        .ok_or_else(|| "entry truncated in its booleans".to_string())?;
    let mut booleans = Vec::with_capacity(boolean_count);
    for flag in flag_bytes {
        match flag {
            0 => booleans.push(false),
            1 => booleans.push(true),
            other => return Err(format!("boolean value {other} is neither absent nor set")),
        }
    }
    at = flags_end;
    if (name_size + boolean_count) % 2 == 1 {
        at = at.checked_add(1).ok_or_else(|| "padding overflows".to_string())?;
    }

    let step = |base: usize, slot: usize| -> Result<usize, String> {
        slot.checked_mul(2)
            .and_then(|scaled| base.checked_add(scaled))
            .ok_or_else(|| "section offset overflow".to_string())
    };
    let mut numbers = Vec::with_capacity(number_count);
    for slot in 0..number_count {
        numbers.push(read_i16(bytes, step(at, slot)?)?);
    }
    at = step(at, number_count)?;

    let mut offsets = Vec::with_capacity(string_count);
    for slot in 0..string_count {
        offsets.push(read_i16(bytes, step(at, slot)?)?);
    }
    at = step(at, string_count)?;

    let table_end = at
        .checked_add(table_size)
        .ok_or_else(|| "string table overflows".to_string())?;
    let table = bytes
        .get(at..table_end)
        .ok_or_else(|| "entry truncated in its string table".to_string())?;

    let mut strings = Vec::with_capacity(string_count);
    for offset in &offsets {
        if *offset < 0 {
            strings.push(None);
            continue;
        }
        let start = usize::try_from(*offset).map_err(|_| "bad string offset".to_string())?;
        let tail = table
            .get(start..)
            .ok_or_else(|| format!("string offset {start} is past the table"))?;
        let end = tail
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| format!("string at {start} is not NUL-terminated"))?;
        let body = tail
            .get(..end)
            .ok_or_else(|| format!("string at {start} is not readable"))?;
        strings.push(Some(body.to_vec()));
    }
    Ok(Decoded {
        names,
        booleans,
        numbers,
        strings,
    })
}

/// The compiled entry the recipe installs.
pub(crate) fn entry() -> Result<Vec<u8>, String> {
    compile(&parse()?)
}

/// The store-relative path the entry is installed at. ncurses looks a terminal
/// up under the first letter of its name.
pub(crate) const INSTALL_PATH: &str = "share/terminfo/t/td-term";

pub(crate) fn selftest() -> Result<(), String> {
    let parsed = parse()?;
    let bytes = entry()?;
    let decoded = decode(&bytes)?;
    if decoded.names != parsed.names {
        return Err("terminfo selftest lost the entry name".into());
    }
    for capability in &parsed.capabilities {
        let found = match &capability.value {
            Value::Flag => decoded.booleans.get(capability.index).copied(),
            Value::Number(value) => decoded
                .numbers
                .get(capability.index)
                .map(|decoded| decoded == value),
            Value::Bytes(bytes) => decoded
                .strings
                .get(capability.index)
                .map(|decoded| decoded.as_deref() == Some(bytes.as_slice())),
        };
        if found != Some(true) {
            return Err(format!(
                "terminfo selftest did not round-trip {}",
                capability.name
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::MOD_SHIFT;
    use crate::keys;
    use std::collections::BTreeSet;


    fn parsed() -> Entry {
        parse().unwrap()
    }

    fn string(entry: &Entry, name: &str) -> Vec<u8> {
        for capability in &entry.capabilities {
            if capability.name != name {
                continue;
            }
            return match &capability.value {
                Value::Bytes(bytes) => bytes.clone(),
                other => panic!("{name} is {other:?}, not a string"),
            };
        }
        panic!("{name} is not in the entry")
    }

    /// The three tables ARE the wire format. Landmarks a reviewer can check
    /// against ncurses' `Caps` without reading all 443 names, at the edges and
    /// either side of the two places the order is famously not alphabetical
    /// (`kf10` between `kf1` and `kf2`; `kf11` after `rfi`, not after `kf10`).
    #[test]
    fn the_capability_tables_are_in_the_pinned_caps_order() {
        assert_eq!(BOOLEANS.len(), 44);
        assert_eq!(NUMBERS.len(), 39);
        // Deliberately truncated one past `setab`; see the table's comment.
        assert_eq!(STRINGS.len(), 361);

        for (name, index) in [("bw", 0), ("am", 1), ("xenl", 4), ("bce", 28), ("OTxr", 43)] {
            assert_eq!(index_of(BOOLEANS, name), Some(index), "boolean {name}");
        }
        for (name, index) in [("cols", 0), ("it", 1), ("colors", 13), ("pairs", 14), ("OTkn", 38)] {
            assert_eq!(index_of(NUMBERS, name), Some(index), "number {name}");
        }
        for (name, index) in [
            ("cbt", 0),
            ("cup", 10),
            ("sgr0", 39),
            ("kf0", 65),
            ("kf1", 66),
            ("kf10", 67),
            ("kf2", 68),
            ("kf9", 75),
            ("smkx", 89),
            ("rep", 121),
            ("acsc", 146),
            ("kcbt", 148),
            ("kend", 164),
            ("rfi", 215),
            ("kf11", 216),
            ("kf63", 268),
            ("el1", 269),
            ("u6", 293),
            ("u9", 296),
            ("op", 297),
            ("sitm", 311),
            ("ritm", 321),
            ("setaf", 359),
            ("setab", 360),
        ] {
            assert_eq!(index_of(STRINGS, name), Some(index), "string {name}");
        }

        // A name repeated in a table would give two capabilities one index and
        // silently drop whichever the encoder reached second.
        for table in [BOOLEANS, NUMBERS, STRINGS] {
            let unique: BTreeSet<&str> = table.iter().copied().collect();
            assert_eq!(unique.len(), table.len(), "a capability name is repeated");
        }
    }

    /// §10's structural check: decode the compiled entry and compare it with
    /// the source capabilities field for field.
    #[test]
    fn the_compiled_entry_decodes_back_to_its_source() {
        let entry = parsed();
        let decoded = decode(&compile(&entry).unwrap()).unwrap();
        assert_eq!(decoded.names, entry.names);

        let mut booleans = 0;
        let mut numbers = 0;
        let mut strings = 0;
        for capability in &entry.capabilities {
            match &capability.value {
                Value::Flag => {
                    booleans += 1;
                    assert_eq!(
                        decoded.booleans.get(capability.index),
                        Some(&true),
                        "boolean {}",
                        capability.name
                    );
                }
                Value::Number(value) => {
                    numbers += 1;
                    assert_eq!(
                        decoded.numbers.get(capability.index),
                        Some(value),
                        "number {}",
                        capability.name
                    );
                }
                Value::Bytes(bytes) => {
                    strings += 1;
                    assert_eq!(
                        decoded.strings.get(capability.index).and_then(Clone::clone),
                        Some(bytes.clone()),
                        "string {}",
                        capability.name
                    );
                }
            }
        }
        assert_eq!(booleans + numbers + strings, entry.capabilities.len());

        // The other direction: every slot the file declares and the source did
        // NOT claim must decode as absent. Without this the comparison above
        // would pass an entry that had grown a capability nobody wrote.
        let claimed: BTreeSet<(u8, usize)> = entry
            .capabilities
            .iter()
            .map(|capability| {
                let kind = match capability.value {
                    Value::Flag => 0,
                    Value::Number(_) => 1,
                    Value::Bytes(_) => 2,
                };
                (kind, capability.index)
            })
            .collect();
        for (index, flag) in decoded.booleans.iter().enumerate() {
            assert_eq!(*flag, claimed.contains(&(0, index)), "boolean slot {index}");
        }
        for (index, value) in decoded.numbers.iter().enumerate() {
            assert_eq!(
                *value != ABSENT,
                claimed.contains(&(1, index)),
                "number slot {index}"
            );
        }
        for (index, value) in decoded.strings.iter().enumerate() {
            assert_eq!(
                value.is_some(),
                claimed.contains(&(2, index)),
                "string slot {index}"
            );
        }
    }

    /// The header's counts and the even-boundary pad are the parts a reader
    /// trusts before it has read anything.
    #[test]
    fn the_binary_layout_is_the_documented_one() {
        let bytes = entry().unwrap();
        assert_eq!(bytes.get(..2), Some(MAGIC.to_le_bytes().as_slice()));
        let header = |slot: usize| read_i16(&bytes, 2 + slot * 2).unwrap() as usize;
        let names = header(0);
        let booleans = header(1);
        let numbers = header(2);
        let strings = header(3);
        let table = header(4);
        // Counts are "one past the highest claimed", not the full standard set.
        assert_eq!(booleans, 29, "highest boolean is bce");
        assert_eq!(numbers, 15, "highest number is pairs");
        assert_eq!(strings, 361, "highest string is setab");
        let pad = usize::from((names + booleans) % 2 == 1);
        assert_eq!(
            bytes.len(),
            12 + names + booleans + pad + numbers * 2 + strings * 2 + table
        );
        // The name section is NUL-terminated and the numbers start even.
        assert_eq!(bytes.get(12 + names - 1), Some(&0));
        assert_eq!((12 + names + booleans + pad) % 2, 0);
    }

    #[test]
    fn the_decoder_refuses_a_corrupt_entry() {
        let good = entry().unwrap();
        assert!(decode(&[]).is_err(), "empty");
        assert!(decode(&good[..8]).is_err(), "truncated header");
        assert!(
            decode(&good[..good.len() - 4]).is_err(),
            "truncated string table"
        );
        let mut wrong_magic = good.clone();
        wrong_magic.splice(0..2, [0x42, 0x42]);
        assert!(decode(&wrong_magic).is_err(), "wrong magic");
        // A boolean byte outside {0,1} is a file another encoder wrote.
        let mut bad_flag = good.clone();
        let names = read_i16(&good, 2).unwrap() as usize;
        bad_flag.splice(12 + names..12 + names + 1, [7]);
        assert!(decode(&bad_flag).is_err(), "boolean out of range");
    }

    #[test]
    fn source_escapes_decode_to_the_bytes_they_name() {
        assert_eq!(unescape(r"\E[1m").unwrap(), b"\x1b[1m");
        assert_eq!(unescape("^G").unwrap(), b"\x07");
        assert_eq!(unescape("^?").unwrap(), b"\x7f");
        assert_eq!(unescape(r"\177").unwrap(), b"\x7f");
        assert_eq!(unescape(r"\s").unwrap(), b" ");
        assert_eq!(unescape(r"\\").unwrap(), b"\\");
        assert!(unescape(r"\q").is_err(), "unknown escape");
        assert!(unescape("^ ").is_err(), "unknown control");
        assert!(unescape(r"\400").is_err(), "octal out of range");
    }

    /// The key capabilities are a promise about bytes ANOTHER module produces.
    /// Nothing but this ties them together: retyping a key in `keys.rs` would
    /// leave this entry telling every curses application the old sequence.
    #[test]
    fn key_capabilities_are_the_bytes_the_adapter_sends() {
        let entry = parsed();
        // ncurses emits `smkx` before reading keys, so the application forms
        // are the ones an application actually sees.
        let application = keys::Modes {
            application_cursor: true,
        };
        let plain = keys::Modes {
            application_cursor: false,
        };
        let send = |name: &str, modifiers: u32, modes: keys::Modes| -> Vec<u8> {
            let code = keys::key_code(name).unwrap_or_else(|| panic!("no key {name}"));
            keys::sequence(code, modifiers, modes)
                .unwrap_or_else(|| panic!("{name} is silent"))
                .as_slice()
                .to_vec()
        };

        for (capability, key) in [
            ("kcuu1", "up"),
            ("kcud1", "down"),
            ("kcuf1", "right"),
            ("kcub1", "left"),
            ("khome", "home"),
            ("kend", "end"),
        ] {
            assert_eq!(string(&entry, capability), send(key, 0, application), "{capability}");
        }
        // Paging and the editing keys are mode-independent, so the entry's one
        // spelling has to be right in BOTH modes.
        for (capability, key) in [
            ("kpp", "pageup"),
            ("knp", "pagedown"),
            ("kich1", "insert"),
            ("kdch1", "delete"),
            ("kbs", "backspace"),
        ] {
            assert_eq!(string(&entry, capability), send(key, 0, plain), "{capability}");
            assert_eq!(
                string(&entry, capability),
                send(key, 0, application),
                "{capability} under DECCKM"
            );
        }
        assert_eq!(string(&entry, "kcbt"), send("tab", MOD_SHIFT, plain));
        for number in 1..=12 {
            let key = format!("f{number}");
            assert_eq!(
                string(&entry, &format!("kf{number}")),
                send(&key, 0, application),
                "kf{number}"
            );
        }
    }

    /// `acsc` pairs an ACS character with the byte to send for it. td-term's
    /// are all identity because `smacs` switches the charset, so the pairs are
    /// exactly the characters the model's DEC map translates — checked against
    /// that map's source, since a glyph added there without a pair here is a
    /// line-drawing character ncurses would not know it could draw.
    #[test]
    fn acsc_covers_exactly_the_models_graphics_map() {
        const TERM: &str = include_str!("term.rs");
        let body = TERM
            .split_once("fn map_charset")
            .and_then(|(_, tail)| tail.split_once("_ => scalar,"))
            .map(|(body, _)| body)
            .expect("map_charset body");
        let mut mapped = BTreeSet::new();
        for line in body.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix('\'') else {
                continue;
            };
            let Some((source, _)) = rest.split_once('\'') else {
                continue;
            };
            let mut characters = source.chars();
            let (Some(character), None) = (characters.next(), characters.next()) else {
                continue;
            };
            // `_` maps to a blank, which is not a drawable ACS glyph.
            if character != '_' {
                mapped.insert(character);
            }
        }
        assert_eq!(mapped.len(), 31, "the DEC map moved");

        let acsc = string(&parsed(), "acsc");
        assert_eq!(acsc.len() % 2, 0, "acsc is a list of pairs");
        let mut paired = BTreeSet::new();
        for pair in acsc.as_chunks::<2>().0 {
            let (Some(acs), Some(sent)) = (pair.first(), pair.get(1)) else {
                continue;
            };
            assert_eq!(acs, sent, "acsc pair {acs:?} is not identity");
            assert!(paired.insert(char::from(*acs)), "acsc lists {acs:?} twice");
        }
        assert_eq!(paired, mapped, "acsc and the DEC map disagree");
    }

    #[test]
    fn the_install_path_is_the_lookup_ncurses_performs() {
        let entry = parsed();
        let name = entry.names.split('|').next().unwrap();
        let letter = name.chars().next().unwrap();
        assert_eq!(INSTALL_PATH, format!("share/terminfo/{letter}/{name}"));
    }

    #[test]
    fn the_source_rejects_malformed_capabilities() {
        // These exercise `parse`'s guards through `unescape`/`index_of`, which
        // is what a hand edit to SOURCE would trip.
        assert!(index_of(STRINGS, "smir").is_some(), "smir exists in Caps");
        assert!(
            !SOURCE.contains("\nstr smir "),
            "this profile has no ANSI insert mode, so it must not claim smir"
        );
        for absent in ["blink", "invis", "cols", "lines"] {
            assert!(
                !SOURCE.contains(&format!(" {absent} ")),
                "{absent} is deliberately not claimed"
            );
        }
    }
}

/// The capabilities whose meaning a corpus citation cannot pin.
///
/// Attribution asks "does the named case exercise this operation". It cannot
/// ask "is this the RIGHT operation for THIS capability", and where a family
/// shares one case the difference is everything: `cursor/absolute-and-relative`
/// writes all four of `CSI A/B/C/D`, so swapping `cuu` and `cud` leaves every
/// attribution satisfied and ships an entry that moves the cursor the wrong
/// way. The same holds for `il1`/`dl1`, `indn`/`rin`, `ich`/`dch`, and the nine
/// renditions that share one case.
///
/// So each is pinned twice more here: its declared spelling must be the same
/// operation as a CONCRETE form written out below, and feeding that concrete
/// form to the model must produce the effect the capability's name promises.
/// The last test refuses to let a capability share a case with another without
/// appearing here.
#[cfg(test)]
mod effects {
    use super::*;
    use crate::term::Terminal;

    /// The concrete form is the capability with its parameters filled in;
    /// `then` is whatever makes the effect observable (a `sc` is only visible
    /// through a later `rc`).
    struct Effect {
        capability: &'static str,
        grid: (usize, usize),
        setup: &'static [u8],
        concrete: &'static [u8],
        then: &'static [u8],
        expect: fn(&Terminal) -> bool,
    }

    fn attributes(terminal: &Terminal) -> crate::term::Attributes {
        terminal
            .cell(0, 0)
            .map(|cell| cell.attributes)
            .unwrap_or_default()
    }

    const EFFECTS: &[Effect] = &[
    Effect {
        capability: "cuu",
        grid: (3, 4),
        setup: b"\x1b[3;1H",
        concrete: b"\x1b[2A",
        then: b"",
        expect: |t| t.cursor().0 == 0,
    },
    Effect {
        capability: "cud",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[2B",
        then: b"",
        expect: |t| t.cursor().0 == 2,
    },
    Effect {
        capability: "cuf",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[2C",
        then: b"",
        expect: |t| t.cursor().1 == 2,
    },
    Effect {
        capability: "cub",
        grid: (3, 4),
        setup: b"\x1b[1;4H",
        concrete: b"\x1b[2D",
        then: b"",
        expect: |t| t.cursor().1 == 1,
    },
    Effect {
        capability: "cuu1",
        grid: (3, 4),
        setup: b"\x1b[2;1H",
        concrete: b"\x1b[A",
        then: b"",
        expect: |t| t.cursor().0 == 0,
    },
    Effect {
        capability: "cud1",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[B",
        then: b"",
        expect: |t| t.cursor().0 == 1,
    },
    Effect {
        capability: "cuf1",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[C",
        then: b"",
        expect: |t| t.cursor().1 == 1,
    },
    Effect {
        capability: "cub1",
        grid: (3, 4),
        setup: b"\x1b[1;3H",
        concrete: b"\x1b[D",
        then: b"",
        expect: |t| t.cursor().1 == 1,
    },
    Effect {
        capability: "cup",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[2;3H",
        then: b"",
        expect: |t| t.cursor() == (1, 2, false),
    },
    Effect {
        capability: "home",
        grid: (3, 4),
        setup: b"\x1b[2;3H",
        concrete: b"\x1b[H",
        then: b"",
        expect: |t| t.cursor() == (0, 0, false),
    },
    Effect {
        capability: "hpa",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[3G",
        then: b"",
        expect: |t| t.cursor().1 == 2,
    },
    Effect {
        capability: "vpa",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[3d",
        then: b"",
        expect: |t| t.cursor().0 == 2,
    },
    Effect {
        capability: "ht",
        grid: (3, 12),
        setup: b"",
        concrete: b"\t",
        then: b"",
        expect: |t| t.cursor().1 == 8,
    },
    Effect {
        capability: "cbt",
        grid: (3, 12),
        setup: b"\x1b[1;10H",
        concrete: b"\x1b[Z",
        then: b"",
        expect: |t| t.cursor().1 == 8,
    },
    Effect {
        capability: "cr",
        grid: (3, 4),
        setup: b"\x1b[1;3H",
        concrete: b"\r",
        then: b"",
        expect: |t| t.cursor().1 == 0,
    },
    Effect {
        capability: "ind",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1bD",
        then: b"",
        expect: |t| t.cursor().0 == 1,
    },
    Effect {
        capability: "ri",
        grid: (3, 4),
        setup: b"\x1b[2;1H",
        concrete: b"\x1bM",
        then: b"",
        expect: |t| t.cursor().0 == 0,
    },
    Effect {
        capability: "nel",
        grid: (3, 4),
        setup: b"\x1b[1;3H",
        concrete: b"\x1bE",
        then: b"",
        expect: |t| t.cursor() == (1, 0, false),
    },
    Effect {
        capability: "sc",
        grid: (3, 4),
        setup: b"\x1b[2;3H",
        concrete: b"\x1b7",
        then: b"\x1b[1;1H\x1b8",
        expect: |t| t.cursor() == (1, 2, false),
    },
    Effect {
        capability: "rc",
        grid: (3, 4),
        setup: b"\x1b[2;3H\x1b7\x1b[1;1H",
        concrete: b"\x1b8",
        then: b"",
        expect: |t| t.cursor() == (1, 2, false),
    },
    Effect {
        capability: "rs1",
        grid: (3, 4),
        setup: b"\x1b[1m",
        concrete: b"\x1bc",
        then: b"X",
        expect: |t| attributes(t) == crate::term::Attributes::default(),
    },
    Effect {
        capability: "il1",
        grid: (3, 4),
        setup: b"AAA\r\nBBB\x1b[1;1H",
        concrete: b"\x1b[L",
        then: b"",
        expect: |t| t.row_text(1).as_deref() == Ok("AAA "),
    },
    Effect {
        capability: "dl1",
        grid: (3, 4),
        setup: b"AAA\r\nBBB\x1b[1;1H",
        concrete: b"\x1b[M",
        then: b"",
        expect: |t| t.row_text(0).as_deref() == Ok("BBB "),
    },
    Effect {
        capability: "il",
        grid: (3, 4),
        setup: b"AAA\r\nBBB\r\nCCC\x1b[1;1H",
        concrete: b"\x1b[2L",
        then: b"",
        expect: |t| t.row_text(2).as_deref() == Ok("AAA "),
    },
    Effect {
        capability: "dl",
        grid: (3, 4),
        setup: b"AAA\r\nBBB\r\nCCC\x1b[1;1H",
        concrete: b"\x1b[2M",
        then: b"",
        expect: |t| t.row_text(0).as_deref() == Ok("CCC "),
    },
    Effect {
        capability: "indn",
        grid: (3, 4),
        setup: b"AAA\r\nBBB",
        concrete: b"\x1b[1S",
        then: b"",
        expect: |t| t.row_text(0).as_deref() == Ok("BBB "),
    },
    Effect {
        capability: "rin",
        grid: (3, 4),
        setup: b"AAA\r\nBBB",
        concrete: b"\x1b[1T",
        then: b"",
        expect: |t| t.row_text(1).as_deref() == Ok("AAA "),
    },
    Effect {
        capability: "ich1",
        grid: (3, 4),
        setup: b"ABC\x1b[1;1H",
        concrete: b"\x1b[@",
        then: b"",
        expect: |t| t.row_text(0).as_deref() == Ok(" ABC"),
    },
    Effect {
        capability: "ich",
        grid: (3, 4),
        setup: b"ABC\x1b[1;1H",
        concrete: b"\x1b[2@",
        then: b"",
        expect: |t| t.row_text(0).as_deref() == Ok("  AB"),
    },
    Effect {
        capability: "dch1",
        grid: (3, 4),
        setup: b"ABC\x1b[1;1H",
        concrete: b"\x1b[P",
        then: b"",
        expect: |t| t.row_text(0).as_deref() == Ok("BC  "),
    },
    Effect {
        capability: "dch",
        grid: (3, 4),
        setup: b"ABC\x1b[1;1H",
        concrete: b"\x1b[2P",
        then: b"",
        expect: |t| t.row_text(0).as_deref() == Ok("C   "),
    },
    Effect {
        capability: "ech",
        grid: (3, 4),
        setup: b"ABC\x1b[1;1H",
        concrete: b"\x1b[2X",
        then: b"",
        expect: |t| t.row_text(0).as_deref() == Ok("  C "),
    },
    Effect {
        capability: "el",
        grid: (3, 4),
        setup: b"ABC\x1b[1;2H",
        concrete: b"\x1b[K",
        then: b"",
        expect: |t| t.row_text(0).as_deref() == Ok("A   "),
    },
    Effect {
        capability: "el1",
        grid: (3, 4),
        setup: b"ABC\x1b[1;2H",
        concrete: b"\x1b[1K",
        then: b"",
        expect: |t| t.row_text(0).as_deref() == Ok("  C "),
    },
    Effect {
        capability: "ed",
        grid: (3, 4),
        setup: b"AAA\r\nBBB\x1b[1;1H",
        concrete: b"\x1b[J",
        then: b"",
        expect: |t| t.row_text(1).as_deref() == Ok("    "),
    },
    Effect {
        capability: "clear",
        grid: (3, 4),
        setup: b"AAA\r\nBBB",
        concrete: b"\x1b[H\x1b[2J",
        then: b"",
        expect: |t| t.row_text(0).as_deref() == Ok("    ") && t.cursor() == (0, 0, false),
    },
    Effect {
        capability: "smacs",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b(0q",
        then: b"",
        expect: |t| t.cell(0, 0).map(|c| c.scalar) == Some('\u{2500}'),
    },
    Effect {
        capability: "rmacs",
        grid: (3, 4),
        setup: b"\x1b(0",
        concrete: b"\x1b(Bq",
        then: b"",
        expect: |t| t.cell(0, 0).map(|c| c.scalar) == Some('q'),
    },
    Effect {
        capability: "bold",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[1mX",
        then: b"",
        expect: |t| attributes(t).bold,
    },
    Effect {
        capability: "dim",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[2mX",
        then: b"",
        expect: |t| attributes(t).faint,
    },
    Effect {
        capability: "sitm",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[3mX",
        then: b"",
        expect: |t| attributes(t).italic,
    },
    Effect {
        capability: "ritm",
        grid: (3, 4),
        setup: b"\x1b[3m",
        concrete: b"\x1b[23mX",
        then: b"",
        expect: |t| !attributes(t).italic,
    },
    Effect {
        capability: "smul",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[4mX",
        then: b"",
        expect: |t| attributes(t).underline && !attributes(t).strike,
    },
    Effect {
        capability: "rmul",
        grid: (3, 4),
        setup: b"\x1b[4m",
        concrete: b"\x1b[24mX",
        then: b"",
        expect: |t| !attributes(t).underline,
    },
    Effect {
        capability: "rev",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[7mX",
        then: b"",
        expect: |t| attributes(t).inverse,
    },
    Effect {
        capability: "smso",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[7mX",
        then: b"",
        expect: |t| attributes(t).inverse,
    },
    Effect {
        capability: "rmso",
        grid: (3, 4),
        setup: b"\x1b[7m",
        concrete: b"\x1b[27mX",
        then: b"",
        expect: |t| !attributes(t).inverse,
    },
    Effect {
        capability: "sgr0",
        grid: (3, 4),
        setup: b"\x1b[1;4;7m",
        concrete: b"\x1b[mX",
        then: b"",
        expect: |t| attributes(t) == crate::term::Attributes::default(),
    },
    Effect {
        capability: "op",
        grid: (3, 4),
        setup: b"\x1b[31;42m",
        concrete: b"\x1b[39;49mX",
        then: b"",
        expect: |t| {
            attributes(t).foreground == crate::term::Color::Default
                && attributes(t).background == crate::term::Color::Default
        },
    },
    Effect {
        capability: "smcup",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[?1049h",
        then: b"",
        expect: |t| t.mode("alternate-screen") == Some(true),
    },
    Effect {
        capability: "rmcup",
        grid: (3, 4),
        setup: b"\x1b[?1049h",
        concrete: b"\x1b[?1049l",
        then: b"",
        expect: |t| t.mode("alternate-screen") == Some(false),
    },
    Effect {
        capability: "smkx",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[?1h",
        then: b"",
        expect: |t| t.mode("application-cursor") == Some(true),
    },
    Effect {
        capability: "rmkx",
        grid: (3, 4),
        setup: b"\x1b[?1h",
        concrete: b"\x1b[?1l",
        then: b"",
        expect: |t| t.mode("application-cursor") == Some(false),
    },
    Effect {
        capability: "smam",
        grid: (3, 4),
        setup: b"\x1b[?7l",
        concrete: b"\x1b[?7h",
        then: b"",
        expect: |t| t.mode("autowrap") == Some(true),
    },
    Effect {
        capability: "rmam",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[?7l",
        then: b"",
        expect: |t| t.mode("autowrap") == Some(false),
    },
    Effect {
        capability: "civis",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[?25l",
        then: b"",
        expect: |t| t.mode("cursor-visible") == Some(false),
    },
    Effect {
        capability: "cnorm",
        grid: (3, 4),
        setup: b"\x1b[?25l",
        concrete: b"\x1b[?25h",
        then: b"",
        expect: |t| t.mode("cursor-visible") == Some(true),
    },
    Effect {
        capability: "csr",
        grid: (3, 4),
        setup: b"AAA\r\nBBB\r\nCCC",
        concrete: b"\x1b[2;3r",
        then: b"\x1b[3;1H\n",
        expect: |t| {
            t.row_text(0).as_deref() == Ok("AAA ") && t.row_text(1).as_deref() == Ok("CCC ")
        },
    },
    Effect {
        capability: "hts",
        grid: (3, 12),
        setup: b"\x1b[1;4H",
        concrete: b"\x1bH",
        then: b"\r\t",
        expect: |t| t.cursor().1 == 3,
    },
    Effect {
        capability: "tbc",
        grid: (3, 12),
        setup: b"",
        concrete: b"\x1b[3g",
        then: b"\r\t",
        expect: |t| t.cursor().1 == 11,
    },
    Effect {
        capability: "rep",
        grid: (3, 4),
        setup: b"A",
        concrete: b"\x1b[2b",
        then: b"",
        expect: |t| t.row_text(0).as_deref() == Ok("AAA "),
    },
    Effect {
        capability: "u7",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[6n",
        then: b"",
        // `u6` is the shape of this answer, so the query has to produce one.
        expect: |t| t.replies() == b"\x1b[1;1R",
    },
    Effect {
        capability: "u9",
        grid: (3, 4),
        setup: b"",
        concrete: b"\x1b[c",
        then: b"",
        // And `u8` is what this query answers, so the entry's advertised
        // device attributes must be the bytes the model actually sends.
        expect: |t| t.replies() == declared("u8").as_slice(),
    },
    ];

    /// The CSI a byte string is, as (private, parameters, final byte), with
    /// terminfo's `%` operators skipped. `None` for a capability that is not a
    /// CSI at all (`\E7`, `\E(0`, `^I`), which has no parameters to fill and
    /// is compared whole instead.
    fn shape(bytes: &[u8]) -> Option<(bool, Vec<u16>, u8)> {
        let mut index = 0;
        while index + 1 < bytes.len() {
            if bytes.get(index) != Some(&0x1b) || bytes.get(index + 1) != Some(&b'[') {
                index += 1;
                continue;
            }
            let mut at = index + 2;
            let private = bytes.get(at) == Some(&b'?');
            if private {
                at += 1;
            }
            let mut parameters = Vec::new();
            let mut digits: Vec<u8> = Vec::new();
            loop {
                match bytes.get(at) {
                    Some(b'%') => {
                        match bytes.get(at + 1) {
                            Some(b'{') => {
                                at += 2;
                                while matches!(bytes.get(at), Some(byte) if *byte != b'}') {
                                    at += 1;
                                }
                                at += 1;
                            }
                            Some(b'p' | b'P' | b'g') => at += 3,
                            Some(_) => at += 2,
                            None => break,
                        }
                        digits.clear();
                    }
                    Some(byte @ b'0'..=b'9') => {
                        digits.push(*byte);
                        at += 1;
                    }
                    Some(b';') => {
                        if !digits.is_empty() {
                            parameters.push(number(&digits));
                            digits.clear();
                        }
                        at += 1;
                    }
                    _ => break,
                }
            }
            if !digits.is_empty() {
                parameters.push(number(&digits));
            }
            match bytes.get(at) {
                Some(final_byte) if (0x40..=0x7e).contains(final_byte) => {
                    return Some((private, parameters, *final_byte))
                }
                _ => index += 1,
            }
        }
        None
    }

    fn number(digits: &[u8]) -> u16 {
        std::str::from_utf8(digits)
            .ok()
            .and_then(|text| text.parse().ok())
            .unwrap_or(0)
    }

    fn declared(name: &str) -> Vec<u8> {
        let entry = parse().unwrap();
        for capability in &entry.capabilities {
            if capability.name != name {
                continue;
            }
            return match &capability.value {
                Value::Bytes(bytes) => bytes.clone(),
                other => panic!("{name} is {other:?}"),
            };
        }
        panic!("{name} is not in the entry")
    }

    /// The concrete form must be the SAME operation the entry declares: same
    /// private flag and final byte, and — where a parameter names the
    /// operation rather than counting — the same parameters. Swapping two
    /// capabilities' spellings moves one of those and reds here.
    #[test]
    fn each_concrete_form_is_the_operation_its_capability_declares() {
        for effect in EFFECTS {
            let spelled = declared(effect.capability);
            match shape(&spelled) {
                None => assert_eq!(
                    effect.concrete.get(..spelled.len()),
                    Some(spelled.as_slice()),
                    "{} is not a CSI, so its concrete form must start with it",
                    effect.capability
                ),
                Some((want_private, want_parameters, want_final)) => {
                    let (private, parameters, final_byte) =
                        shape(effect.concrete).unwrap_or_else(|| {
                            panic!("{}'s concrete form is not a CSI", effect.capability)
                        });
                    assert_eq!(
                        (private, final_byte),
                        (want_private, want_final),
                        "{} concrete operation",
                        effect.capability
                    );
                    if final_byte == b'm' || private {
                        assert_eq!(
                            parameters, want_parameters,
                            "{} concrete parameters",
                            effect.capability
                        );
                    }
                }
            }
        }
    }

    /// And the model must actually do what the capability's name says.
    #[test]
    fn the_model_performs_what_each_capability_names() {
        for effect in EFFECTS {
            let mut terminal = Terminal::new(effect.grid.0, effect.grid.1).unwrap();
            terminal.feed(effect.setup);
            terminal.feed(effect.concrete);
            terminal.feed(effect.then);
            assert!(
                (effect.expect)(&terminal),
                "{} did not do what it names",
                effect.capability
            );
        }
    }

    /// Enough of terminfo's parameter language to expand `setaf`/`setab`.
    /// Colour is where a swapped branch is invisible — it still produces a
    /// well-formed SGR, just for the wrong channel — so the branches are
    /// expanded and driven through the model rather than attributed.
    /// `%? c1 %t b1 %e c2 %t b2 %e b3 %;` is ONE conditional with an else-if
    /// chain, so a taken branch has to suppress every later `%t`.
    fn tparm(format: &[u8], argument: u16) -> Vec<u8> {
        let mut output = Vec::new();
        let mut stack: Vec<i64> = Vec::new();
        let mut emitting = true;
        let mut satisfied = false;
        let mut index = 0;
        while let Some(byte) = format.get(index) {
            if *byte != b'%' {
                if emitting {
                    output.push(*byte);
                }
                index += 1;
                continue;
            }
            let operator = format.get(index + 1).copied().unwrap_or(b'%');
            index += 2;
            match operator {
                b'%' => {
                    if emitting {
                        output.push(b'%');
                    }
                }
                // Operands and arithmetic run in both branches; only output is
                // suppressed, as ncurses evaluates them.
                b'p' => {
                    index += 1;
                    stack.push(i64::from(argument));
                }
                b'{' => {
                    let mut value: i64 = 0;
                    while let Some(digit @ b'0'..=b'9') = format.get(index) {
                        value = value * 10 + i64::from(digit - b'0');
                        index += 1;
                    }
                    index += 1;
                    stack.push(value);
                }
                b'<' => {
                    let right = stack.pop().unwrap_or(0);
                    let left = stack.pop().unwrap_or(0);
                    stack.push(i64::from(left < right));
                }
                b'-' => {
                    let right = stack.pop().unwrap_or(0);
                    let left = stack.pop().unwrap_or(0);
                    stack.push(left - right);
                }
                b'd' => {
                    let value = stack.pop().unwrap_or(0);
                    if emitting {
                        output.extend_from_slice(value.to_string().as_bytes());
                    }
                }
                b'?' => satisfied = false,
                b't' => {
                    let taken = stack.pop().unwrap_or(0) != 0;
                    emitting = !satisfied && taken;
                    if emitting {
                        satisfied = true;
                    }
                }
                b'e' => emitting = !satisfied,
                b';' => emitting = true,
                _ => {}
            }
        }
        output
    }

    /// Every branch of `setaf`/`setab` must set the channel its name promises.
    #[test]
    fn the_colour_capabilities_reach_the_channel_they_name() {
        use crate::term::Color;
        for (capability, foreground) in [("setaf", true), ("setab", false)] {
            let format = declared(capability);
            // One colour from each branch: the 30-37 form, the 90-97 bright
            // form, and the 38;5 palette form.
            for colour in [3u16, 10, 200] {
                let expanded = tparm(&format, colour);
                let mut terminal = Terminal::new(2, 2).unwrap();
                terminal.feed(&expanded);
                terminal.feed(b"X");
                let attributes = terminal
                    .cell(0, 0)
                    .map(|cell| cell.attributes)
                    .unwrap_or_default();
                let (reached, untouched) = if foreground {
                    (attributes.foreground, attributes.background)
                } else {
                    (attributes.background, attributes.foreground)
                };
                assert_eq!(
                    reached,
                    Color::Indexed(colour as u8),
                    "{capability} {colour} reached the wrong colour ({expanded:?})"
                );
                assert_eq!(
                    untouched,
                    Color::Default,
                    "{capability} {colour} disturbed the other channel"
                );
            }
        }
    }

    /// A capability that shares its case with another is permutable unless it
    /// is pinned above, so sharing without an effect check is refused.
    #[test]
    fn every_capability_sharing_a_case_has_an_effect_check() {
        use std::collections::BTreeMap;
        let entry = parse().unwrap();
        let mut by_case: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for capability in &entry.capabilities {
            if matches!(capability.value, Value::Bytes(_)) {
                by_case
                    .entry(capability.case)
                    .or_default()
                    .push(capability.name);
            }
        }
        let covered: Vec<&str> = EFFECTS.iter().map(|effect| effect.capability).collect();
        // Keys are pinned byte-for-byte against the adapter. `u6` is the
        // shape of `u7`'s answer and `u8` is `u9`'s, both asserted against the
        // model's own reply stream in EFFECTS above. `setaf`/`setab` are
        // expanded; `acsc` is a table; `rs2` is `rs1` by construction.
        let exempt = |name: &str| {
            name.starts_with('k')
                || name == "u6"
                || name == "u8"
                || name == "acsc"
                || name == "setaf"
                || name == "setab"
        };
        let mut unpinned = Vec::new();
        for (case, names) in &by_case {
            if names.len() < 2 {
                continue;
            }
            for name in names {
                if !covered.contains(name) && !exempt(name) {
                    unpinned.push(format!("{name} (shares {case})"));
                }
            }
        }
        assert!(
            unpinned.is_empty(),
            "capabilities share a case with nothing pinning which is which: {}",
            unpinned.join(", ")
        );
    }
}
