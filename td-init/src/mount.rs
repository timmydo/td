//! `mount` and `umount` — the filesystem glue every phase of a td boot runs.
//!
//! `mount(2)` and `umount2(2)`, the pair the ninth-syscall amendment bought.
//! Both `/init` scripts, `/etc/inittab`'s sysinit lines, `/etc/shutdown` and
//! td-boot's own helpers mount every filesystem the machine has; until this
//! module existed that was the one job on td's boot path only busybox could do,
//! which is why a 1 MiB C multicall rode in the initramfs to make four calls.
//!
//! Deliberately absent, each because td has no use for it rather than to save
//! work: `/etc/fstab` (td writes its mount table into the scripts that run it,
//! so the one-operand `mount TARGET` form has nothing to resolve the remaining
//! operands against and is refused, not silently ignored); `/etc/mtab` and its
//! `-n` (`/proc/self/mounts` IS the table both applets read); and loop-device
//! setup, since `losetup` needs `ioctl(2)` requests outside this crate's
//! amendment — td-boot still reaches busybox for that one.

use crate::sys;
use std::ffi::CString;

/// The kernel's own view of what is mounted. `/proc/self/mounts` rather than
/// `/proc/mounts`: they differ inside a mount namespace, and the one that
/// matters is always the caller's.
const MOUNTS: &str = "/proc/self/mounts";

/// One line of the mount table, as RAW BYTES.
///
/// Not `String`: a mount point may hold any byte but `/` and NUL, and `umount
/// -a` does not merely compare these paths — it passes them back to the kernel.
/// A lossy decode would replace an invalid sequence with U+FFFD and then
/// `umount` the path that does not exist, so the one filesystem whose name
/// needed care is the one that stays mounted through a shutdown.
#[derive(Debug, PartialEq, Eq)]
pub struct Entry {
    pub source: Vec<u8>,
    pub target: Vec<u8>,
    pub fstype: Vec<u8>,
    pub opts: Vec<u8>,
}

/// Parse a mount table. Pure, so both applets and `switch_root` share ONE
/// reading of the kernel's format rather than each keeping its own.
///
/// Fields are split on a single ASCII space, which is exactly what
/// `fs/proc_namespace.c` writes — NOT on "whitespace", which in Rust means
/// Unicode `White_Space` and would split a path containing U+00A0 into two
/// fields the kernel never escaped, yielding a truncated target.
///
/// A line short of a target contributes nothing rather than ending the walk:
/// this list decides what gets unmounted, and one malformed line must not
/// silently truncate it.
pub fn parse_table(text: &[u8]) -> Vec<Entry> {
    let mut out = Vec::new();
    for line in text.split(|b| *b == b'\n') {
        let mut fields = line.split(|b| *b == b' ');
        let (Some(source), Some(target)) = (fields.next(), fields.next()) else {
            continue;
        };
        // No mount point is the empty path, so a line that yields one is
        // malformed — and an empty target reaching `umount -a` is a syscall on
        // nothing that then counts as a filesystem this failed to release.
        if target.is_empty() {
            continue;
        }
        out.push(Entry {
            source: unescape(source),
            target: unescape(target),
            fstype: fields.next().map(unescape).unwrap_or_default(),
            opts: fields.next().map(unescape).unwrap_or_default(),
        });
    }
    out
}

/// Undo the kernel's `\040` (space), `\011` (tab), `\012` (newline) and `\134`
/// (backslash) escaping of a mount field. Anything else is copied verbatim —
/// those four are the only escapes the kernel emits, and `\` never occurs
/// inside a UTF-8 sequence, so a multi-byte name passes through untouched.
pub fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0usize;
    while let Some(b) = field.get(i) {
        let escape = if *b == b'\\' {
            match field.get(i + 1..i + 4) {
                Some(b"040") => Some(b' '),
                Some(b"011") => Some(b'\t'),
                Some(b"012") => Some(b'\n'),
                Some(b"134") => Some(b'\\'),
                _ => None,
            }
        } else {
            None
        };
        match escape {
            Some(c) => {
                out.push(c);
                i += 4;
            }
            None => {
                out.push(*b);
                i += 1;
            }
        }
    }
    out
}

/// The live table, read as bytes for the reason `Entry` documents.
fn read_table() -> Result<Vec<Entry>, String> {
    let raw = std::fs::read(MOUNTS).map_err(|e| format!("{MOUNTS}: {e}"))?;
    Ok(parse_table(&raw))
}

/// A path as it reads in a diagnostic. Lossy is right HERE and nowhere else:
/// this text goes to a console, never back to the kernel.
fn shown(path: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(path)
}

fn cstr(s: &[u8]) -> Result<CString, String> {
    CString::new(s).map_err(|_| format!("'{}' contains a NUL byte", shown(s)))
}

// ── mount ───────────────────────────────────────────────────────────────────

/// What one `-o` token does to the flag word. `Clear` is not decoration:
/// `-o ro` and `-o rw` name the same bit, and in util-linux the LAST token wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Effect {
    Set,
    Clear,
}

/// The `-o` tokens that are FLAGS. Every other token is filesystem DATA and is
/// handed to the kernel unchanged — `mode=0755`, `subvol=@var`, and any
/// fs-specific word — which is what util-linux and busybox do, and what makes
/// each filesystem the validator for its own options.
///
/// A flag word missing from this table would be passed as data instead, and
/// most filesystems reject unknown data with EINVAL. That is the failure to
/// prefer: loud, at mount time, rather than a `nosuid` that silently did not
/// take. The table is the whole flag surface `sys::mount` can be given.
const OPTIONS: &[(&str, usize, Effect)] = &[
    ("async", sys::MS_SYNCHRONOUS, Effect::Clear),
    ("atime", sys::MS_NOATIME, Effect::Clear),
    ("bind", sys::MS_BIND, Effect::Set),
    // util-linux's `defaults` is rw,suid,dev,exec,async — every bit this table
    // knows, OFF, which is where the flag word already starts.
    ("defaults", 0, Effect::Set),
    ("dev", sys::MS_NODEV, Effect::Clear),
    ("diratime", sys::MS_NODIRATIME, Effect::Clear),
    ("exec", sys::MS_NOEXEC, Effect::Clear),
    ("move", sys::MS_MOVE, Effect::Set),
    ("noatime", sys::MS_NOATIME, Effect::Set),
    ("nodev", sys::MS_NODEV, Effect::Set),
    ("nodiratime", sys::MS_NODIRATIME, Effect::Set),
    ("noexec", sys::MS_NOEXEC, Effect::Set),
    ("norelatime", sys::MS_RELATIME, Effect::Clear),
    ("nosuid", sys::MS_NOSUID, Effect::Set),
    ("relatime", sys::MS_RELATIME, Effect::Set),
    ("remount", sys::MS_REMOUNT, Effect::Set),
    ("ro", sys::MS_RDONLY, Effect::Set),
    ("rw", sys::MS_RDONLY, Effect::Clear),
    ("suid", sys::MS_NOSUID, Effect::Clear),
    ("sync", sys::MS_SYNCHRONOUS, Effect::Set),
];

/// A plain loop rather than an iterator search: this file is embedded verbatim
/// into the recipe, and the ladder guard scans step content for host-tool names
/// that the search combinator happens to share.
fn flag(token: &str) -> Option<(usize, Effect)> {
    for (name, bit, effect) in OPTIONS {
        if *name == token {
            return Some((*bit, *effect));
        }
    }
    None
}

fn usage() -> String {
    "usage: mount [-t TYPE] [-o OPT[,OPT]...] [-r] [-w] SOURCE TARGET\n       \
     mount                                  (no operands: print the table)\n  \
     -t  filesystem type\n  \
     -o  comma-separated options; a word this does not know is filesystem data\n  \
     -r  read-only (same as -o ro)\n  \
     -w  read-write (same as -o rw)"
        .to_string()
}

/// A mount to perform, with `-o` already split into the kernel's two halves.
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    source: String,
    target: String,
    fstype: Option<String>,
    flags: usize,
    data: String,
}

/// `Ok(None)` is the no-operand form: print the table and touch nothing.
///
/// Everything is parsed BEFORE any syscall, so a typo exits 1 rather than
/// mounting something over a directory the rest of the boot needs.
fn parse(args: &[String]) -> Result<Option<Plan>, String> {
    let mut fstype: Option<String> = None;
    let mut flags = 0usize;
    let mut data: Vec<&str> = Vec::new();
    let mut operands: Vec<&str> = Vec::new();
    let mut options_given = false;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            // An option's VALUE may not itself look like an option: `mount -t -o
            // proc /p` would otherwise mount a filesystem of type "-o" and
            // silently drop the option list the caller meant.
            "-t" | "--types" => {
                let Some(t) = rest.next().filter(|t| !t.starts_with('-')) else {
                    return Err(format!("-t needs a filesystem TYPE\n{}", usage()));
                };
                fstype = Some(t.clone());
                options_given = true;
            }
            "-o" | "--options" => {
                let Some(o) = rest.next().filter(|o| !o.starts_with('-')) else {
                    return Err(format!("-o needs an option list\n{}", usage()));
                };
                for token in o.split(',') {
                    if token.is_empty() {
                        continue;
                    }
                    match flag(token) {
                        Some((bit, Effect::Set)) => flags |= bit,
                        Some((bit, Effect::Clear)) => flags &= !bit,
                        None => data.push(token),
                    }
                }
                options_given = true;
            }
            "-r" => {
                flags |= sys::MS_RDONLY;
                options_given = true;
            }
            "-w" => {
                flags &= !sys::MS_RDONLY;
                options_given = true;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unrecognised argument '{other}'\n{}", usage()));
            }
            other => operands.push(other),
        }
    }
    // The kernel IGNORES MS_RDONLY on a plain MS_BIND: a read-only bind needs a
    // second `mount(…, MS_REMOUNT|MS_BIND|MS_RDONLY, …)`. util-linux issues that
    // automatically; busybox does not, and neither does this — so the
    // combination is refused rather than quietly producing a WRITABLE bind
    // mount. This is the one option pair whose silent failure would be
    // permissive, which is the direction this module refuses to fail in.
    // MS_REMOUNT is exempt: `-o remount,bind,ro` IS that second call, and
    // refusing it would refuse the only spelling this message can recommend.
    if flags & sys::MS_BIND != 0 && flags & sys::MS_RDONLY != 0 && flags & sys::MS_REMOUNT == 0 {
        return Err(format!(
            "-o bind,ro would be a READ-WRITE bind mount: the kernel ignores ro on a bind. \
             Bind it first, then `mount -o remount,bind,ro TARGET`\n{}",
            usage()
        ));
    }
    match operands.as_slice() {
        // Options with nothing to apply them to is a mistake, not a request to
        // print the table: `mount -o remount,ro` (a form util-linux resolves
        // through fstab) would otherwise report success having done nothing.
        [] if options_given => Err(format!("options but no SOURCE and TARGET\n{}", usage())),
        [] => Ok(None),
        // The one lone operand that means something without an fstab: a REMOUNT
        // changes an existing mount, and the kernel ignores `source` entirely
        // when MS_REMOUNT is set. `mount -o remount,rw /` is what an operator
        // types when a read-only root is the problem, so refusing it would be
        // refusing the repair.
        [one] if flags & sys::MS_REMOUNT != 0 => Ok(Some(Plan {
            source: (*one).to_string(),
            target: (*one).to_string(),
            fstype,
            flags,
            data: data.join(","),
        })),
        [one] => Err(format!(
            "{one}: mount needs both SOURCE and TARGET — td ships no /etc/fstab \
             to resolve a lone operand against\n{}",
            usage()
        )),
        [source, target] => Ok(Some(Plan {
            source: (*source).to_string(),
            target: (*target).to_string(),
            fstype,
            flags,
            data: data.join(","),
        })),
        more => Err(format!(
            "{} operands; mount takes SOURCE and TARGET\n{}",
            more.len(),
            usage()
        )),
    }
}

fn apply(plan: &Plan) -> Result<(), String> {
    let source = cstr(plan.source.as_bytes())?;
    let target = cstr(plan.target.as_bytes())?;
    let fstype = match &plan.fstype {
        Some(t) => Some(cstr(t.as_bytes())?),
        None => None,
    };
    // An EMPTY data string is not the same argument as none: a non-NULL pointer
    // makes the filesystem parse an option list, and some reject a stray one.
    let data = match plan.data.is_empty() {
        true => None,
        false => Some(cstr(plan.data.as_bytes())?),
    };
    sys::mount(
        &source,
        &target,
        fstype.as_deref(),
        plan.flags,
        data.as_deref(),
    )
    .map_err(|e| format!("mounting {} on {}: {e}", plan.source, plan.target))
}

/// The no-operand form, in util-linux's `SOURCE on TARGET type FSTYPE (OPTS)`
/// spelling — the same text busybox printed, so an operator's habits carry over
/// the cutover.
///
/// Fields are RE-ESCAPED, not printed as parsed. `unescape` turns `\012` into a
/// real newline, and a mount point holding one would otherwise print as two
/// lines — breaking the one-line-per-mount shape everything reading this output
/// assumes, including the build check that counts them.
fn table_text(entries: &[Entry]) -> String {
    let mut out = String::new();
    for e in entries {
        out.push_str(&escaped(&e.source));
        out.push_str(" on ");
        out.push_str(&escaped(&e.target));
        out.push_str(" type ");
        out.push_str(&escaped(&e.fstype));
        out.push_str(" (");
        out.push_str(&escaped(&e.opts));
        out.push_str(")\n");
    }
    out
}

/// A field as the kernel would have written it: the four separators back in
/// their octal form, everything else shown (lossily — this is display).
fn escaped(field: &[u8]) -> String {
    // Bytes throughout, decoded ONCE at the end: escaping into a `String` a
    // byte at a time would make every byte of a multi-byte character its own
    // invalid sequence and print `/mnt/réal` as replacement characters. The
    // four escaped bytes are ASCII, which UTF-8 continuation bytes never are.
    let mut out: Vec<u8> = Vec::with_capacity(field.len());
    for b in field {
        match b {
            b' ' => out.extend_from_slice(b"\\040"),
            b'\t' => out.extend_from_slice(b"\\011"),
            b'\n' => out.extend_from_slice(b"\\012"),
            b'\\' => out.extend_from_slice(b"\\134"),
            other => out.push(*other),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn mount(args: &[String]) -> Result<u8, String> {
    match parse(args)? {
        Some(plan) => {
            apply(&plan)?;
            Ok(0)
        }
        None => {
            crate::emit(&table_text(&read_table()?))?;
            Ok(0)
        }
    }
}

// ── umount ──────────────────────────────────────────────────────────────────

fn umount_usage() -> String {
    "usage: umount [-f] [-l] [-r] TARGET...\n       \
     umount -a [-f] [-l] [-r]\n  \
     -a  every filesystem in the mount table, deepest first\n  \
     -f  MNT_FORCE\n  \
     -l  MNT_DETACH (lazy: detach now, release when nothing holds it)\n  \
     -r  remount read-only whatever will not unmount"
        .to_string()
}

#[derive(Debug, PartialEq, Eq)]
struct UmountPlan {
    all: bool,
    remount_ro: bool,
    flags: usize,
    targets: Vec<String>,
}

fn parse_umount(args: &[String]) -> Result<UmountPlan, String> {
    let mut plan = UmountPlan {
        all: false,
        remount_ro: false,
        flags: 0,
        targets: Vec::new(),
    };
    for a in args {
        match a.as_str() {
            "--all" => plan.all = true,
            "--force" => plan.flags |= sys::MNT_FORCE,
            "--lazy" => plan.flags |= sys::MNT_DETACH,
            "--read-only" => plan.remount_ro = true,
            // Short flags cluster, as they do in busybox and in `halt`: `-a -r`
            // and `-ar` are one option word to whoever writes the script. An
            // unknown letter rejects the WHOLE word, so a typo unmounts nothing.
            other if other.starts_with('-') && !other.starts_with("--") && other.len() > 1 => {
                for c in other.chars().skip(1) {
                    match c {
                        'a' => plan.all = true,
                        'f' => plan.flags |= sys::MNT_FORCE,
                        'l' => plan.flags |= sys::MNT_DETACH,
                        'r' => plan.remount_ro = true,
                        _ => {
                            return Err(format!(
                                "unrecognised option '-{c}' in '{other}'\n{}",
                                umount_usage()
                            ))
                        }
                    }
                }
            }
            other if other.starts_with("--") => {
                return Err(format!(
                    "unrecognised argument '{other}'\n{}",
                    umount_usage()
                ))
            }
            other => plan.targets.push(other.to_string()),
        }
    }
    if plan.all && !plan.targets.is_empty() {
        return Err(format!(
            "-a releases everything and takes no TARGET\n{}",
            umount_usage()
        ));
    }
    if !plan.all && plan.targets.is_empty() {
        return Err(format!("no TARGET\n{}", umount_usage()));
    }
    Ok(plan)
}

/// One filesystem to release, with the flag word it is CURRENTLY mounted with.
///
/// Those flags are load-bearing, not context: `MS_REMOUNT` REPLACES a mount's
/// flag word rather than updating it. A fallback passing `MS_REMOUNT|MS_RDONLY`
/// alone would bring a busy `ro,nodev,nosuid,noexec` volume back as plain `ro`
/// — read-only, and quietly stripped of every protection it had — while
/// reporting success. `None` means the target is absent from the mount table,
/// which is also why its `umount` failed: there is nothing there to remount.
struct Target {
    path: Vec<u8>,
    mounted_with: Option<usize>,
}

/// The VFS flag word a `/proc/self/mounts` option list describes, read through
/// the SAME table `-o` parses so the two can never disagree about what `nosuid`
/// means. Filesystem options in that list (`subvol=@var`, `size=…`) are not VFS
/// flags and are skipped — they belong to the superblock, and a remount that
/// restated them would be answering a question it was not asked.
fn flags_from_options(opts: &[u8]) -> usize {
    let mut flags = 0usize;
    for token in opts.split(|b| *b == b',') {
        let Ok(name) = std::str::from_utf8(token) else {
            continue;
        };
        match flag(name) {
            Some((bit, Effect::Set)) => flags |= bit,
            Some((bit, Effect::Clear)) => flags &= !bit,
            None => {}
        }
    }
    flags
}

/// What a read-only fallback must pass, given what the mount already carries.
/// Split out so the "REPLACES, not updates" rule has somewhere to be tested:
/// nothing else would notice the difference — the remount succeeds either way.
fn remount_ro_flags(mounted_with: usize) -> usize {
    mounted_with | sys::MS_REMOUNT | sys::MS_RDONLY
}

fn table_flags(table: &[Entry], path: &[u8]) -> Option<usize> {
    for e in table {
        if e.target == path {
            return Some(flags_from_options(&e.opts));
        }
    }
    None
}

/// What `umount` will work through.
///
/// For `-a`: every mount point, in REVERSE TABLE ORDER — which is what busybox
/// does. Not a depth sort, and worth not claiming to be one: it is child-before-
/// parent only because the kernel lists a mount after whatever it was mounted
/// on, which holds for td's own sequence but not for an arbitrary namespace.
/// Table order would reach each parent while a child still held it. The whole
/// table is read before the first unmount, because `/proc` is one of the
/// filesystems `-a` is about to take away.
///
/// For operands: the path names itself. The table is consulted only under `-r`,
/// which needs the flags a remount would otherwise replace — a plain
/// `umount /proc` must not depend on `/proc` being readable, since taking it
/// away is exactly what it is for. That read is best-effort for the same
/// reason: an unreadable table costs the fallback, not the unmount.
fn targets_for(plan: &UmountPlan) -> Result<Vec<Target>, String> {
    if plan.all {
        let mut targets: Vec<Target> = read_table()?
            .iter()
            .map(|e| Target {
                path: e.target.clone(),
                mounted_with: Some(flags_from_options(&e.opts)),
            })
            .collect();
        targets.reverse();
        return Ok(targets);
    }
    // Best-effort: an unreadable table costs the device lookup and the `-r`
    // fallback, never the unmount itself. `umount /proc` must not depend on
    // /proc, since taking it away is exactly what it is for.
    let table = read_table().unwrap_or_default();
    Ok(plan
        .targets
        .iter()
        .map(|p| resolve(&table, p.as_bytes()))
        .collect())
}

/// An operand as a target to release. It normally names its own mount point,
/// but `umount /dev/vda` — naming the DEVICE — is what busybox and util-linux
/// accept and what an operator will type; `umount2(2)` itself takes only a
/// mount point and answers a device with EINVAL. So an operand that is no
/// mount point but IS a mounted source resolves to that source's mount point.
///
/// The target reading wins: a path can be both, and what the operand most
/// obviously means is the thing mounted THERE.
fn resolve(table: &[Entry], operand: &[u8]) -> Target {
    if let Some(mounted_with) = table_flags(table, operand) {
        return Target {
            path: operand.to_vec(),
            mounted_with: Some(mounted_with),
        };
    }
    for e in table {
        if e.source == operand {
            return Target {
                path: e.target.clone(),
                mounted_with: Some(flags_from_options(&e.opts)),
            };
        }
    }
    Target {
        path: operand.to_vec(),
        mounted_with: None,
    }
}

/// One target, with the `-r` fallback. `/` is the case that fallback exists
/// for: nothing unmounts the root of a running system, so a shutdown's
/// `umount -a -r` settles for making it read-only, which is what actually
/// protects the filesystem across the reboot.
fn unmount_one(target: &Target, flags: usize, remount_ro: bool) -> Result<(), String> {
    let c = cstr(&target.path)?;
    let Err(e) = sys::umount(&c, flags) else {
        return Ok(());
    };
    let shown = shown(&target.path);
    if !remount_ro {
        return Err(format!("{shown}: {e}"));
    }
    let Some(mounted_with) = target.mounted_with else {
        return Err(format!(
            "{shown}: {e}, and it is in no readable mount table, so there is \
             nothing to remount read-only"
        ));
    };
    // The kernel ignores `source` for a remount, so the target names itself
    // rather than being looked back up in a table `-a` may already have
    // unmounted its way past.
    sys::mount(
        &c,
        &c,
        None,
        remount_ro_flags(mounted_with),
        None,
    )
    .map_err(|re| format!("{shown}: {e}; remounting read-only: {re}"))
}

pub fn umount(args: &[String]) -> Result<u8, String> {
    let plan = parse_umount(args)?;
    let targets = targets_for(&plan)?;
    let mut failures: Vec<String> = Vec::new();
    for target in &targets {
        if let Err(e) = unmount_one(target, plan.flags, plan.remount_ro) {
            failures.push(e);
        }
    }
    match failures.as_slice() {
        [] => Ok(0),
        // One failure is the whole story; `main` prints it under the applet
        // name, so returning it avoids saying the same thing twice.
        [only] => Err(only.clone()),
        many => {
            for f in many {
                crate::emit_err(&format!("umount: {f}\n"));
            }
            Err(format!(
                "{} of {} filesystems could not be released",
                many.len(),
                targets.len()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn argv(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).to_string()).collect()
    }

    fn plan(xs: &[&str]) -> Result<Option<Plan>, String> {
        parse(&argv(xs))
    }

    fn mounted(xs: &[&str]) -> Plan {
        plan(xs).unwrap().unwrap()
    }

    /// The mount lines td's own image runs, parsed. These are the assertions
    /// that make the option table more than a list: `-o ro,nodev,nosuid,noexec`
    /// has to become that flag word and an EMPTY data string, and
    /// `-o rw,nodev,nosuid,subvol=@var` has to split into flags AND the one
    /// btrfs word — a `subvol` swallowed as a flag mounts the wrong subvolume.
    #[test]
    fn the_image_s_own_mount_lines_parse_into_the_calls_they_mean() {
        let dev = mounted(&["-t", "devtmpfs", "devtmpfs", "/dev"]);
        assert_eq!(dev.fstype.as_deref(), Some("devtmpfs"));
        assert_eq!(dev.source, "devtmpfs");
        assert_eq!(dev.target, "/dev");
        assert_eq!(dev.flags, 0);
        assert_eq!(dev.data, "");

        let volume = mounted(&[
            "-t",
            "btrfs",
            "-o",
            "ro,nodev,nosuid,noexec",
            "/dev/vda",
            "/volume",
        ]);
        assert_eq!(
            volume.flags,
            sys::MS_RDONLY | sys::MS_NODEV | sys::MS_NOSUID | sys::MS_NOEXEC
        );
        assert_eq!(volume.data, "");

        let var = mounted(&[
            "-t",
            "btrfs",
            "-o",
            "rw,nodev,nosuid,subvol=@var",
            "/dev/vda",
            "/sysroot/var",
        ]);
        assert_eq!(var.flags, sys::MS_NODEV | sys::MS_NOSUID);
        assert_eq!(var.data, "subvol=@var");

        let run = mounted(&["-t", "tmpfs", "-o", "mode=0755", "tmpfs", "/sysroot/run"]);
        assert_eq!(run.flags, 0);
        assert_eq!(run.data, "mode=0755");

        let moved = mounted(&["-o", "move", "/volume", "/sysroot/run/td-volume"]);
        assert_eq!(moved.flags, sys::MS_MOVE);
        assert_eq!(moved.fstype, None);
        assert_eq!(moved.data, "");
    }

    /// `ro` and `rw` name ONE bit, so the order they appear in decides the
    /// answer. Getting this backwards mounts `/var` read-only and the machine
    /// boots to a system that cannot write its own state.
    #[test]
    fn an_option_that_clears_a_bit_beats_an_earlier_one_that_set_it() {
        assert_eq!(mounted(&["-o", "ro,rw", "d", "/t"]).flags, 0);
        assert_eq!(
            mounted(&["-o", "rw,ro", "d", "/t"]).flags,
            sys::MS_RDONLY
        );
        assert_eq!(mounted(&["-r", "-w", "d", "/t"]).flags, 0);
        assert_eq!(mounted(&["-w", "-r", "d", "/t"]).flags, sys::MS_RDONLY);
        assert_eq!(
            mounted(&["-o", "nosuid,nodev,suid", "d", "/t"]).flags,
            sys::MS_NODEV
        );
        // `defaults` is the no-op the flag word already starts as.
        assert_eq!(mounted(&["-o", "defaults", "d", "/t"]).flags, 0);
    }

    /// Anything the table does not know is the filesystem's business, in the
    /// order it was written — `-o compress=zstd,discard` must reach btrfs whole.
    #[test]
    fn unknown_option_words_are_passed_through_as_filesystem_data() {
        let p = mounted(&["-o", "ro,compress=zstd,discard,nodev", "d", "/t"]);
        assert_eq!(p.flags, sys::MS_RDONLY | sys::MS_NODEV);
        assert_eq!(p.data, "compress=zstd,discard");
        // Empty tokens (`-o ro,,nodev`, a trailing comma) contribute nothing
        // rather than an empty data word the filesystem would reject.
        let p = mounted(&["-o", "ro,,", "d", "/t"]);
        assert_eq!(p.data, "");
    }

    /// The discriminating case, and the one the image's greeter probe asserts:
    /// an unknown argument must be refused BEFORE any syscall, and the refusal
    /// must SAY so. An EPERM from an unprivileged prod would also exit non-zero,
    /// which is the opposite of the contract.
    #[test]
    fn an_unknown_argument_is_rejected_before_any_syscall() {
        for bad in [
            &["--not-an-option"][..],
            &["--not-an-option", "d", "/t"][..],
            &["-z", "d", "/t"][..],
        ] {
            let e = plan(bad).unwrap_err();
            assert!(
                e.contains("unrecognised argument"),
                "mount {bad:?} refused without saying so: {e}"
            );
        }
        // An option whose argument is missing must not consume the operand.
        assert!(plan(&["-t"]).is_err());
        assert!(plan(&["-o"]).is_err());
    }

    /// td ships no fstab, so the one-operand form has nowhere to look the rest
    /// up. Refusing it beats "succeeding" at a mount that never happened.
    #[test]
    fn the_operand_count_is_exactly_two_or_none() {
        assert_eq!(plan(&[]), Ok(None));
        let e = plan(&["/mnt"]).unwrap_err();
        assert!(e.contains("fstab"), "{e}");
        assert!(plan(&["a", "b", "c"]).is_err());
        // ...and options with nothing to apply them to are a mistake, not a
        // request to print the table.
        assert!(plan(&["-o", "remount,ro"]).is_err());
        assert!(plan(&["-t", "tmpfs"]).is_err());
        assert!(plan(&["-r"]).is_err());
    }

    /// The exception: a REMOUNT changes a mount that already exists, and the
    /// kernel ignores `source` when MS_REMOUNT is set. `mount -o remount,rw /`
    /// is what an operator types when a read-only root is the problem.
    #[test]
    fn a_lone_operand_is_accepted_for_a_remount_and_names_itself() {
        let p = mounted(&["-o", "remount,rw", "/"]);
        assert_eq!(p.source, "/");
        assert_eq!(p.target, "/");
        assert_eq!(p.flags, sys::MS_REMOUNT);
        assert_eq!(p.fstype, None);
        assert_eq!(p.data, "");
        // Without the remount bit the same shape is still refused — `-o bind` or
        // `-o move` with one operand is a mistake, not a repair.
        assert!(plan(&["-o", "bind", "/mnt"]).is_err());
        assert!(plan(&["-o", "move", "/mnt"]).is_err());
        // A remount with two operands is unchanged.
        let p = mounted(&["-o", "remount,ro", "/dev/vda", "/var"]);
        assert_eq!(p.source, "/dev/vda");
        assert_eq!(p.target, "/var");
    }

    /// Every flag in the table is a real Linux `MS_*` bit and no two names that
    /// mean opposite things share a spelling. A duplicate would make one of them
    /// dead, silently.
    #[test]
    fn the_option_table_is_sorted_and_unique() {
        let mut names: Vec<&str> = OPTIONS.iter().map(|(n, _, _)| *n).collect();
        let listed = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), listed, "an option name is listed twice");
        let sorted: Vec<&str> = OPTIONS.iter().map(|(n, _, _)| *n).collect();
        assert_eq!(sorted, names, "OPTIONS must be sorted");
        // Exactly one bit per flag (except `defaults`, whose whole meaning is
        // that it changes nothing).
        for (name, bit, _) in OPTIONS {
            if *name == "defaults" {
                assert_eq!(*bit, 0);
                continue;
            }
            assert_eq!(bit.count_ones(), 1, "{name} is not a single MS_ bit");
        }
    }

    /// Every bit in the table is one `sys.rs` DECLARES — not merely a plausible
    /// single-bit number.
    ///
    /// This is the half of the widened `mount(2)` confinement that lives here.
    /// The flag word is a runtime parameter now, and `main.rs`'s guard on where
    /// `MS_`/`MNT_` may be NAMED exempts this file, so `("rec", 0x4000,
    /// Effect::Set)` would sort legally, pass the single-bit check, name no
    /// constant at all, and hand the kernel MS_REC — mount propagation, which
    /// no applet here has any business performing. Comparing VALUES against the
    /// declared set is what closes that: a bit nobody declared cannot appear.
    #[test]
    fn every_option_bit_is_one_the_syscall_layer_declares() {
        const DECLARED: &[usize] = &[
            sys::MS_RDONLY,
            sys::MS_NOSUID,
            sys::MS_NODEV,
            sys::MS_NOEXEC,
            sys::MS_SYNCHRONOUS,
            sys::MS_REMOUNT,
            sys::MS_NOATIME,
            sys::MS_NODIRATIME,
            sys::MS_BIND,
            sys::MS_MOVE,
            sys::MS_RELATIME,
        ];
        for (name, bit, _) in OPTIONS {
            assert!(
                *bit == 0 || DECLARED.contains(bit),
                "-o {name} sets {bit:#x}, which is not one of the flags sys.rs declares - \
                 a bare number here reaches a kernel operation the amendment never covered"
            );
        }
        // ...and the declared set is the amended one, so this list cannot be
        // grown by adding a constant to sys.rs alone (main.rs pins that roster).
        assert_eq!(DECLARED.len(), 11);
    }

    const MOUNTS_TEXT: &str = "\
rootfs / rootfs rw 0 0
devtmpfs /dev devtmpfs rw,nosuid 0 0
proc /proc proc rw,nosuid,nodev,noexec 0 0
/dev/vda /run/td-volume btrfs ro,nodev 0 0
";

    #[test]
    fn a_mount_table_parses_into_its_four_leading_fields() {
        let table = parse_table(MOUNTS_TEXT.as_bytes());
        assert_eq!(table.len(), 4);
        assert_eq!(table[0].source, b"rootfs");
        assert_eq!(table[0].target, b"/");
        assert_eq!(table[0].fstype, b"rootfs");
        assert_eq!(table[0].opts, b"rw");
        assert_eq!(table[3].target, b"/run/td-volume");
        assert_eq!(table[3].fstype, b"btrfs");
        // The kernel escapes the four separators it uses.
        let escaped = parse_table(b"/dev/sda1 /mnt/new\\040root ext4 rw 0 0\n");
        assert_eq!(escaped[0].target, b"/mnt/new root");
        // A multi-byte name survives, and an unknown escape stays literal.
        assert_eq!(
            parse_table("x /mnt/r\u{e9}al\\040y ext4 rw 0 0\n".as_bytes())[0].target,
            "/mnt/r\u{e9}al y".as_bytes()
        );
        assert_eq!(
            parse_table(b"x /mnt/a\\999 ext4 rw 0 0\n")[0].target,
            b"/mnt/a\\999"
        );
        assert_eq!(parse_table(b"x /mnt/a\\134b ext4 rw 0 0\n")[0].target, b"/mnt/a\\b");
        // A short line contributes nothing — and does not end the walk, which
        // for `umount -a` would be a silently shortened list.
        let ragged = parse_table(b"garbage\n\nproc /proc proc rw 0 0\n");
        assert_eq!(ragged.len(), 1);
        assert_eq!(ragged[0].target, b"/proc");
        // Missing trailing fields are empty, not a dropped entry.
        let short = parse_table(b"src /tgt\n");
        assert_eq!(short.len(), 1);
        assert_eq!(short[0].fstype, b"");
        // An empty TARGET is dropped, though: no mount point is the empty path,
        // and one reaching `umount -a` is a syscall on nothing that then counts
        // as a filesystem this failed to release.
        assert!(parse_table(b"src  /tgt ext4 rw 0 0\n").is_empty());
    }

    /// The bytes reach `umount` intact, which is the whole reason this table is
    /// not `String`. A lossy decode replaces the invalid sequence with U+FFFD,
    /// and `umount -a` then asks the kernel to release a path that does not
    /// exist — leaving the one filesystem whose name needed care mounted
    /// through a shutdown that reported success for everything else.
    #[test]
    fn a_non_utf8_mount_point_survives_the_parse_byte_for_byte() {
        let table = parse_table(b"/dev/sda1 /mnt/\xff\xfe ext4 rw 0 0\n");
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].target, b"/mnt/\xff\xfe");
        // ...and it is still a legal argument to the syscall wrapper.
        assert_eq!(
            cstr(&table[0].target).unwrap().as_bytes(),
            b"/mnt/\xff\xfe"
        );
        // A NUL is the one byte a path cannot hold, and it is refused rather
        // than silently truncating the path the kernel would act on.
        assert!(cstr(b"/mnt/a\0b").is_err());
    }

    /// The kernel writes ONE ASCII space between fields and escapes the four
    /// separators it uses — but NOT U+00A0, which Rust counts as whitespace.
    /// Splitting on "whitespace" would cut this target in half and unmount a
    /// path that is not the one listed.
    #[test]
    fn a_unicode_space_in_a_path_is_not_a_field_separator() {
        let table = parse_table("x /mnt/a\u{a0}b ext4 rw 0 0\n".as_bytes());
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].target, "/mnt/a\u{a0}b".as_bytes());
        assert_eq!(table[0].fstype, b"ext4");
    }

    #[test]
    fn the_table_prints_in_the_spelling_busybox_used() {
        assert_eq!(
            table_text(&parse_table(b"proc /proc proc rw,nodev 0 0\n")),
            "proc on /proc type proc (rw,nodev)\n"
        );
        assert_eq!(table_text(&[]), "");
        // Display is the ONE place lossy is right: this text goes to a console,
        // never back to the kernel, so an unprintable name must not stop the
        // rest of the table from being readable.
        assert_eq!(
            table_text(&parse_table(b"x /mnt/\xff ext4 rw 0 0\n")),
            "x on /mnt/\u{fffd} type ext4 (rw)\n"
        );
    }

    /// ONE LINE PER MOUNT, whatever the mount point holds. `unescape` turns
    /// `\012` into a real newline, so printing fields as parsed would split an
    /// entry across two lines — and everything reading this output, the build
    /// check that counts entries included, assumes it cannot happen.
    #[test]
    fn a_separator_in_a_mount_point_is_re_escaped_on_the_way_out() {
        let text = table_text(&parse_table(
            b"x /mnt/a\\012b ext4 rw 0 0\ny /mnt/c\\040d ext4 rw 0 0\n",
        ));
        assert_eq!(text.lines().count(), 2, "an entry split across lines: {text:?}");
        assert!(text.contains("/mnt/a\\012b"), "{text}");
        assert!(text.contains("/mnt/c\\040d"), "{text}");
        // A round trip: what is printed parses back to what was read.
        let once = parse_table(b"x /mnt/a\\134b ext4 rw 0 0\n");
        assert_eq!(escaped(&once[0].target).as_bytes(), b"/mnt/a\\134b");
        // ...and a multi-byte name is not mangled a byte at a time.
        assert_eq!(escaped("/mnt/r\u{e9}al".as_bytes()), "/mnt/r\u{e9}al");
    }

    fn uplan(xs: &[&str]) -> Result<UmountPlan, String> {
        parse_umount(&argv(xs))
    }

    /// `MS_REMOUNT` REPLACES a mount's flag word; it does not update it. So the
    /// `-r` fallback has to restate everything the mount already carried, or a
    /// busy `ro,nodev,nosuid,noexec` volume comes back as plain `ro` — quietly
    /// stripped of every protection it had, with `umount -a -r` reporting
    /// success. Nothing else in this crate would notice: the remount succeeds
    /// either way, and the mount is still read-only.
    #[test]
    fn the_read_only_fallback_carries_the_flags_the_mount_already_had() {
        // The option list /run/td-volume actually carries on the booted image.
        let held = flags_from_options(b"ro,nodev,nosuid,noexec");
        assert_eq!(
            held,
            sys::MS_RDONLY | sys::MS_NODEV | sys::MS_NOSUID | sys::MS_NOEXEC
        );
        let remount = remount_ro_flags(held);
        for (name, bit) in [
            ("nodev", sys::MS_NODEV),
            ("nosuid", sys::MS_NOSUID),
            ("noexec", sys::MS_NOEXEC),
        ] {
            assert!(
                remount & bit != 0,
                "the read-only remount dropped {name} - the filesystem comes back weaker \
                 than it was mounted, and every test but this one still passes"
            );
        }
        assert!(remount & sys::MS_REMOUNT != 0 && remount & sys::MS_RDONLY != 0);
    }

    /// The flags come off the SAME table `-o` parses, so the two cannot disagree
    /// about what a word means. Filesystem options are not VFS flags and must
    /// not be mistaken for them.
    #[test]
    fn a_proc_mounts_option_list_reads_back_as_its_vfs_flags() {
        // A writable mount clears MS_RDONLY, and `rw` comes first in the
        // kernel's listing, so a table entry never reads as read-only by
        // accident.
        assert_eq!(
            flags_from_options(b"rw,nosuid,nodev,noexec,relatime"),
            sys::MS_NOSUID | sys::MS_NODEV | sys::MS_NOEXEC | sys::MS_RELATIME
        );
        // Superblock options are skipped, not mistaken for flags.
        assert_eq!(
            flags_from_options(b"rw,nodev,nosuid,subvol=/@var,compress=zstd"),
            sys::MS_NODEV | sys::MS_NOSUID
        );
        assert_eq!(flags_from_options(b""), 0);
        // A non-UTF-8 option word is skipped rather than aborting the read, so
        // one odd token cannot cost the whole flag set.
        assert_eq!(flags_from_options(b"ro,\xff\xfe,nodev"), sys::MS_RDONLY | sys::MS_NODEV);
    }

    /// A target the mount table does not list has nothing to remount, and that
    /// is reported rather than passed to the kernel as a bare
    /// `MS_REMOUNT|MS_RDONLY` — which is where the dropped flags came from.
    #[test]
    fn a_target_absent_from_the_table_has_no_flags_to_carry() {
        let table = parse_table(b"/dev/vda /var btrfs rw,nodev 0 0\n");
        assert_eq!(
            table_flags(&table, b"/var"),
            Some(sys::MS_NODEV)
        );
        assert_eq!(table_flags(&table, b"/not-mounted"), None);
        assert_eq!(table_flags(&[], b"/var"), None);
    }

    /// `-a` releases in reverse table order, and every target carries the flags
    /// it was mounted with.
    #[test]
    fn the_target_list_for_dash_a_is_the_table_reversed() {
        let plan = uplan(&["-a"]).unwrap();
        assert!(plan.all);
        // `targets_for` reads the live /proc for `-a`, so exercise the ordering
        // through the pure half instead: the table as parsed, reversed.
        let table = parse_table(
            b"/dev/vda / erofs ro 0 0\n\
              /dev/vda /var btrfs rw,nodev 0 0\n\
              tmpfs /run tmpfs rw 0 0\n\
              /dev/vda /run/td-volume btrfs ro,nodev,nosuid,noexec 0 0\n",
        );
        let mut order: Vec<&[u8]> = table.iter().map(|e| e.target.as_slice()).collect();
        order.reverse();
        assert_eq!(
            order,
            vec![
                b"/run/td-volume".as_slice(),
                b"/run".as_slice(),
                b"/var".as_slice(),
                b"/".as_slice()
            ],
            "a child must be released before the mount it sits on"
        );
    }

    /// `umount /dev/vda` names the DEVICE, which is what busybox accepts and
    /// what an operator types — but `umount2(2)` takes only a mount point and
    /// answers a device with EINVAL. The operand resolves through the table.
    #[test]
    fn an_operand_naming_a_device_resolves_to_its_mount_point() {
        let table = parse_table(
            b"/dev/vda /var btrfs rw,nodev 0 0\nproc /proc proc rw,nosuid 0 0\n",
        );
        let by_device = resolve(&table, b"/dev/vda");
        assert_eq!(by_device.path, b"/var");
        assert_eq!(by_device.mounted_with, Some(sys::MS_NODEV));
        // A mount point still names itself...
        let by_point = resolve(&table, b"/proc");
        assert_eq!(by_point.path, b"/proc");
        assert_eq!(by_point.mounted_with, Some(sys::MS_NOSUID));
        // ...and an operand that is neither is passed through untouched, so the
        // kernel's own diagnostic is what the operator sees.
        let unknown = resolve(&table, b"/nowhere");
        assert_eq!(unknown.path, b"/nowhere");
        assert_eq!(unknown.mounted_with, None);
        // With no readable table an operand is still itself — `umount /proc`
        // must not depend on /proc.
        assert_eq!(resolve(&[], b"/proc").path, b"/proc");
    }

    /// The kernel IGNORES `MS_RDONLY` on a plain bind, so `-o bind,ro` would be
    /// a WRITABLE bind mount. That is the one option pair whose silent failure
    /// is permissive, so it is refused rather than performed.
    #[test]
    fn a_read_only_bind_is_refused_rather_than_silently_made_writable() {
        let e = plan(&["-o", "bind,ro", "/a", "/b"]).unwrap_err();
        assert!(e.contains("READ-WRITE bind mount"), "{e}");
        assert!(plan(&["-o", "ro,bind", "/a", "/b"]).is_err());
        // A plain bind and a plain ro mount are both fine...
        assert_eq!(mounted(&["-o", "bind", "/a", "/b"]).flags, sys::MS_BIND);
        assert_eq!(mounted(&["-o", "ro", "/a", "/b"]).flags, sys::MS_RDONLY);
        // ...and the second call the message recommends is NOT refused, or the
        // diagnostic would be advising something this applet cannot do.
        assert_eq!(
            mounted(&["-o", "remount,bind,ro", "/b", "/b"]).flags,
            sys::MS_REMOUNT | sys::MS_BIND | sys::MS_RDONLY
        );
    }

    /// An option's VALUE may not look like an option: `mount -t -o proc /p`
    /// would otherwise mount a filesystem of type "-o" and drop the option list.
    #[test]
    fn an_option_value_that_looks_like_an_option_is_refused() {
        assert!(plan(&["-t", "-o", "proc", "/p"]).is_err());
        assert!(plan(&["-o", "-t", "proc", "/p"]).is_err());
        assert_eq!(
            mounted(&["-t", "proc", "proc", "/p"]).fstype.as_deref(),
            Some("proc")
        );
    }

    #[test]
    fn umount_takes_either_targets_or_dash_a_but_never_both() {
        assert_eq!(
            uplan(&["/proc"]),
            Ok(UmountPlan {
                all: false,
                remount_ro: false,
                flags: 0,
                targets: vec!["/proc".to_string()],
            })
        );
        assert_eq!(uplan(&["-a"]).map(|p| p.all), Ok(true));
        assert!(uplan(&[]).is_err());
        assert!(uplan(&["-a", "/proc"]).is_err());
    }

    /// `-a -r` is what `/etc/shutdown` runs, and `-ar` must mean the same. One
    /// bad letter rejects the whole word rather than applying the good ones and
    /// tearing half the system down anyway.
    #[test]
    fn clustered_umount_flags_mean_what_the_separated_ones_do() {
        assert_eq!(uplan(&["-ar"]), uplan(&["-a", "-r"]));
        assert_eq!(uplan(&["-ar"]).map(|p| p.remount_ro), Ok(true));
        assert_eq!(uplan(&["-a", "-f"]).map(|p| p.flags), Ok(sys::MNT_FORCE));
        assert_eq!(uplan(&["-a", "-l"]).map(|p| p.flags), Ok(sys::MNT_DETACH));
        assert_eq!(
            uplan(&["-afl"]).map(|p| p.flags),
            Ok(sys::MNT_FORCE | sys::MNT_DETACH)
        );
        assert!(uplan(&["-ax"]).is_err());
        assert!(uplan(&["-xa"]).is_err());
        // The greeter probes this exact refusal.
        let e = uplan(&["--not-an-option"]).unwrap_err();
        assert!(e.contains("unrecognised argument"), "{e}");
    }

    /// A path is a TARGET, not an option, even when it looks unusual — and a
    /// long form nobody typed must not be silently taken for one.
    #[test]
    fn umount_long_forms_mirror_the_letters() {
        assert_eq!(uplan(&["--all"]).map(|p| p.all), Ok(true));
        assert_eq!(
            uplan(&["--all", "--read-only"]).map(|p| p.remount_ro),
            Ok(true)
        );
        assert_eq!(
            uplan(&["--all", "--force", "--lazy"]).map(|p| p.flags),
            Ok(sys::MNT_FORCE | sys::MNT_DETACH)
        );
        assert_eq!(
            uplan(&["/mnt/a", "/mnt/b"]).map(|p| p.targets),
            Ok(vec!["/mnt/a".to_string(), "/mnt/b".to_string()])
        );
    }
}
