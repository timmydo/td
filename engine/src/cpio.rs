//! newc cpio ARCHIVE WRITING (`070701`), pure `std` — enough of the format to
//! build the small appendix td concatenates onto an initramfs, and no more.
//!
//! There is no reader here and no general archiver. What this exists for is one
//! job, described in `td-install/DESIGN.md` §6: a harness must put a per-run
//! trusted public key inside the SELECTOR initramfs — the one firmware loads,
//! in whose rootfs the td-boot that VERIFIES runs — without any recipe being
//! parameterized. Linux accepts CONCATENATED cpio archives, so the key rides
//! in a second archive appended to the recipe-built one. "The initramfs
//! td-boot is in" is what this used to say, and it is ambiguous in the one way
//! that matters: td-boot is in both of them.
//!
//! ## The format, as the pinned kernel's own producer writes it
//!
//! Checked against `linux-7.1.4` — `usr/gen_init_cpio.c`, which is the program
//! the recipes actually run, and `init/initramfs.c`, which is what parses the
//! result. A header is 110 bytes: the six-byte magic, then THIRTEEN fields of
//! exactly eight uppercase hex digits each, in this order
//! (`usr/gen_init_cpio.c:436-451`):
//!
//! ```text
//! ino mode uid gid nlink mtime filesize
//! devmajor devminor rdevmajor rdevminor namesize chksum
//! ```
//!
//! `namesize` counts the trailing NUL. The name follows the header, then NUL
//! padding to a 4-byte boundary; file data follows, then padding to 4 again
//! (`padlen(offset, 4)`, `:454-456` and `:487`). The archive ends with a
//! `TRAILER!!!` entry whose fields are all zero except `nlink = 1` and
//! `namesize = 11` (`:87-118`) — note its device numbers are 0, where a real
//! entry's are 3 and 1.
//!
//! One deliberate divergence: `gen_init_cpio` pads its trailer out to a 512
//! multiple (`:112`), and this pads to 4. That padding exists to leave a
//! standalone archive on a block boundary for whatever follows it; an appendix
//! has nothing following it, and the kernel eats trailing NULs either way
//! (`do_reset`, `init/initramfs.c:324-331`). Four hundred wasted bytes in every
//! initramfs is a poor trade for matching a number that means nothing here.
//!
//! ## Determinism
//!
//! Every field is a constant or derived from the entry: `mtime` is pinned to 1,
//! the value the recipes pass `gen_init_cpio` as `-t 1`, so both halves of a
//! concatenation agree; `uid`/`gid` are 0; inode numbers count up from 721 as
//! `gen_init_cpio`'s do. The same entries therefore always produce the same
//! bytes, so the only thing that varies between two runs is what the caller
//! put IN the archive.
//!
//! Which side of the trust boundary that lands on is the caller's to get right
//! and is worth stating here, because an earlier draft of this file got it
//! backwards. td's appendix goes onto a private copy of the SELECTOR
//! initramfs — the artifact firmware loads, whose rootfs runs the td-boot that
//! verifies. Its modified hash is recorded nowhere: the selector's own manifest
//! describes the recipe output and is checked BEFORE the append. It is
//! emphatically NOT appended to a deployment's `initramfs.cpio`, which the
//! deployment manifest hashes and whose digest is the deployment id — a key
//! there would be inside the artifact being authenticated, and would make the
//! id depend on the key.
//!
//! ## Why the appendix works at all, and what it rests on
//!
//! Three properties of `init/initramfs.c`, each read rather than assumed:
//!
//! - A trailer does NOT stop the parse. `do_name` frees the hardlink table and
//!   returns, and the driver loop keeps going (`:367-377`), which is what lets a
//!   second archive follow a complete first one.
//! - The magic of the next archive must sit at a 4-ALIGNED offset: the restart
//!   branch is guarded by `!(this_header & 3)` (`:532-546`). NUL bytes between
//!   archives are skipped one at a time, so padding is free — but it must be
//!   NUL, and it must land the magic on 4. What a MISALIGNED appendix costs is
//!   worth spelling out, because it is not the loud failure it looks like:
//!   `do_reset` eats the NUL run and then errors `broken padding` if the offset
//!   it stopped at is unaligned (`:324-331`), which sets `message` — and the
//!   driver loop is `while (!message && len)`, so `decompress_method` and its
//!   `invalid magic` are never reached. Where that error then goes depends on
//!   which archive it came from, and td's SELECTOR initramfs — the one this
//!   appendix rides, and the one firmware loads — is an INITRD
//!   (`CONFIG_INITRAMFS_SOURCE=""`, qemu `-initrd`) rather than the built-in
//!   one: it takes `:726-733`, not the `panic_show_mem` at `:714-716`. With
//!   `CONFIG_BLK_DEV_RAM` off — allnoconfig leaves it off and the recipe's
//!   delta list does not turn it on — that arm is a lone `printk(KERN_EMERG
//!   "Initramfs unpacking failed: %s\n")` and THE BOOT CONTINUES, base archive
//!   extracted, key absent. So `alignment_padding` is not belt-and-braces over
//!   a kernel check that would catch a mistake here; it is the only thing
//!   between a misaligned appendix and a machine that boots without its trust
//!   root.
//! - A later regular file REPLACES an earlier one: `clean_path` then
//!   `filp_open(..., O_TRUNC)` (`:378-395`). `O_TRUNC` is applied whenever
//!   `maybe_link` did not report an existing link, and `maybe_link` engages only
//!   at `nlink >= 2` (`:346-348`) — so with the `nlink = 1` written here, an
//!   inode number colliding with one in the base archive cannot silently turn
//!   this file into a hardlink to that one. (The trailer frees the table anyway.)
//!
//! ## What is refused rather than written
//!
//! Absolute names, `.`/`..` components, empty names, non-ASCII, control bytes,
//! a name of `TRAILER!!!`, permission bits outside `0o7777`, and anything too
//! large for a field or longer than `PATH_MAX`. `gen_init_cpio` silently STRIPS
//! a leading slash (`:127-128`); this refuses instead, because a writer that
//! quietly places a file somewhere other than where it was asked to is the
//! wrong shape for a thing that installs a trust root. Control bytes are in
//! that list for a duller reason than the rest: nothing in the format objects
//! to a newline, but the recipes assert archive membership with `grep -q -x -F`
//! (`recipes/src/ladder.rs:440`), which a name containing one silently breaks.
//!
//! The limit of all of them is worth stating, since it bounds what the harness
//! may assume. These rules constrain the NAME; they cannot constrain where the
//! kernel RESOLVES it, which is against the rootfs as it stands when the entry
//! is read — a symlinked `etc` already extracted from the base archive would
//! redirect a `.`-free, relative, perfectly well-formed name. Nor does a
//! missing parent announce itself: `filp_open` failing is `return 0`
//! (`init/initramfs.c:386-387`), the same silent absence an over-long name
//! gets. So an appendix carries its own directory entries rather than relying
//! on the base archive's.

/// `070701`: the newc format without a checksum field in use. `070702` would
/// make the last field a real byte-sum the kernel verifies; this writes 0 there
/// and must therefore not claim to be `070702`.
const MAGIC: &[u8; 6] = b"070701";
const HEADER_LEN: usize = 110;
const TRAILER_NAME: &str = "TRAILER!!!";
/// The mtime the recipes pin with `gen_init_cpio -t 1`.
const MTIME: u32 = 1;
/// `gen_init_cpio`'s own starting inode (`usr/gen_init_cpio.c:34`).
const FIRST_INO: u32 = 721;
/// The device a real entry claims, and which the trailer does not.
const DEV_MAJOR: u32 = 3;
const DEV_MINOR: u32 = 1;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const PERMISSION_BITS: u32 = 0o7777;
/// Linux's `PATH_MAX`, which bounds `namesize` INCLUDING its NUL.
///
/// This is a refusal rather than a formatting detail, because of how the kernel
/// declines: `do_header` has already set `state = SkipIt` when it tests
/// `name_len <= 0 || name_len > PATH_MAX` (`init/initramfs.c:293-297`), so an
/// over-long name is skipped in SILENCE — no diagnostic, boot continues, and
/// the file simply is not there. For a writer whose job is installing a trust
/// root that is the worst available failure: everything reports success and the
/// key is absent.
const PATH_MAX: usize = 4096;

/// What an entry is. The TYPE bits of `mode` come from this rather than from the
/// caller, so a directory cannot be asked for with a regular file's bits.
pub enum Kind<'a> {
    Directory,
    File(&'a [u8]),
}

pub struct Entry<'a> {
    /// Relative, no leading slash — the spelling `cpio -t` lists and the
    /// kernel resolves against the rootfs.
    pub name: &'a str,
    /// Permission bits only (`0o7777`).
    pub mode: u32,
    pub kind: Kind<'a>,
}

/// The archive bytes for `entries`, trailer included.
///
/// The result is meant to be APPENDED to an existing initramfs. The caller must
/// ensure the append point is 4-byte aligned; `gen_init_cpio` pads its own
/// trailer out to a 512 multiple, so a recipe-built archive already is.
pub fn build(entries: &[Entry]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut ino = FIRST_INO;
    for entry in entries {
        check_name(entry.name)?;
        if entry.mode & !PERMISSION_BITS != 0 {
            return Err(format!(
                "cpio: mode {:#o} for {} sets bits outside 0o7777; the type bits are the writer's",
                entry.mode, entry.name
            ));
        }
        let (mode, nlink, data) = match &entry.kind {
            // nlink 2 for a directory is what `gen_init_cpio` writes
            // (`usr/gen_init_cpio.c:195`); the kernel does not check it.
            Kind::Directory => (S_IFDIR | entry.mode, 2u32, &[][..]),
            // nlink 1 is load-bearing rather than cosmetic — see the module
            // header: it is what keeps `maybe_link` out of the path entirely.
            Kind::File(data) => (S_IFREG | entry.mode, 1u32, *data),
        };
        let size = u32::try_from(data.len())
            .map_err(|_| format!("cpio: {} is too large for a newc filesize field", entry.name))?;
        push_header(
            &mut out, ino, mode, nlink, MTIME, size, entry.name, DEV_MAJOR, DEV_MINOR,
        )?;
        out.extend_from_slice(data);
        pad_to(&mut out, 4);
        ino = ino.saturating_add(1);
    }
    // The trailer takes 0 for mtime as well as for its device numbers: EVERY
    // field but `nlink` and `namesize` is zero there (`usr/gen_init_cpio.c:87-118`),
    // which is why mtime is a parameter rather than the constant it was — a
    // header routine shared with real entries writes 1 here otherwise, and no
    // reader would complain.
    push_header(&mut out, 0, 0, 1, 0, 0, TRAILER_NAME, 0, 0)?;
    Ok(out)
}

/// NUL padding that carries `bytes` to a 4-aligned length, for a caller
/// appending this archive to one whose length it does not control.
///
/// Separate from `build` because the alignment that matters is the one at the
/// JOIN, which is a property of the file being appended to rather than of the
/// archive being appended.
#[must_use]
pub fn alignment_padding(length: usize) -> usize {
    (4 - (length % 4)) % 4
}

fn check_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("cpio: an entry name may not be empty".to_string());
    }
    if name == TRAILER_NAME {
        return Err(format!(
            "cpio: {TRAILER_NAME} is the end-of-archive marker and may not be an entry name"
        ));
    }
    if !name.is_ascii() {
        return Err(format!("cpio: entry name {name:?} is not ASCII"));
    }
    if name.contains('\0') {
        return Err(format!("cpio: entry name {name:?} contains a NUL"));
    }
    // Not a format constraint — newc would carry a newline happily. It is the
    // recipes' `grep -q -x -F` membership assertions that a name containing one
    // would quietly defeat.
    if let Some(byte) = name.bytes().find(u8::is_ascii_control) {
        return Err(format!(
            "cpio: entry name {name:?} contains the control byte {byte:#04x}"
        ));
    }
    if name.starts_with('/') {
        return Err(format!(
            "cpio: entry name {name:?} is absolute; names are relative to the rootfs"
        ));
    }
    // `.` and `..` are refused rather than resolved: this writer places a trust
    // root, and a name that walks upward is one whose destination is not the
    // one the caller wrote down.
    for component in name.split('/') {
        if component == "." || component == ".." {
            return Err(format!("cpio: entry name {name:?} contains a {component:?} component"));
        }
        if component.is_empty() {
            return Err(format!("cpio: entry name {name:?} has an empty component"));
        }
    }
    if name.len().saturating_add(1) > PATH_MAX {
        return Err(format!(
            "cpio: entry name is {} bytes with its NUL, over PATH_MAX ({PATH_MAX}); \
             the kernel would skip it in silence",
            name.len().saturating_add(1)
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // one per newc header field that varies
fn push_header(
    out: &mut Vec<u8>,
    ino: u32,
    mode: u32,
    nlink: u32,
    mtime: u32,
    size: u32,
    name: &str,
    dev_major: u32,
    dev_minor: u32,
) -> Result<(), String> {
    let namesize = u32::try_from(name.len().saturating_add(1))
        .map_err(|_| format!("cpio: entry name {name:?} is too long for a newc namesize field"))?;
    let before = out.len();
    out.extend_from_slice(MAGIC);
    for field in [
        ino,
        mode,
        0,          // uid
        0,          // gid
        nlink,
        mtime,
        size,
        dev_major,
        dev_minor,
        0,          // rdevmajor
        0,          // rdevminor
        namesize,
        0,          // chksum: zero, and only honest because the magic is 070701
    ] {
        out.extend_from_slice(&hex8(field));
    }
    if out.len().saturating_sub(before) != HEADER_LEN {
        return Err(format!(
            "cpio: header for {name:?} came to {} bytes, not {HEADER_LEN}",
            out.len().saturating_sub(before)
        ));
    }
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    pad_to(out, 4);
    Ok(())
}

fn pad_to(out: &mut Vec<u8>, align: usize) {
    let padding = if align == 0 { 0 } else { (align - (out.len() % align)) % align };
    out.resize(out.len().saturating_add(padding), 0);
}

/// Eight uppercase hex digits, the width every newc field has.
fn hex8(value: u32) -> [u8; 8] {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = [b'0'; 8];
    let mut remaining = value;
    for slot in out.iter_mut().rev() {
        *slot = *DIGITS
            .get((remaining & 0xf) as usize)
            .unwrap_or(&b'0');
        remaining >>= 4;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(archive: &[u8], header_at: usize, index: usize) -> u32 {
        let start = header_at + 6 + index * 8;
        let text = std::str::from_utf8(&archive[start..start + 8]).unwrap();
        u32::from_str_radix(text, 16).unwrap()
    }

    fn name_at(archive: &[u8], header_at: usize, namesize: usize) -> String {
        let start = header_at + HEADER_LEN;
        String::from_utf8(archive[start..start + namesize - 1].to_vec()).unwrap()
    }

    /// The field ORDER is the thing a reader cannot recover from the bytes: a
    /// swapped pair is a well-formed header describing a different file. So it
    /// is pinned by offset against the layout `usr/gen_init_cpio.c` writes.
    ///
    /// Every expectation here is a LITERAL rather than the constant the writer
    /// used, which is the whole point: `field(…) == MTIME` agrees with itself
    /// however wrong `MTIME` is, and a review sweep found six constants pinned
    /// only that way. Two of the six were sharp — `070702` turns the last field
    /// into a checksum the kernel verifies against the 0 written there
    /// (`init/initramfs.c:283-284`, `:422`), and a type nibble of `0o120000`
    /// makes `do_header` take the `S_ISLNK` branch (`:298`) so the key's BYTES
    /// become a symlink target.
    #[test]
    fn header_fields_sit_at_their_spec_offsets() {
        let archive = build(&[Entry {
            name: "etc/td/deployment.pub",
            mode: 0o644,
            kind: Kind::File(b"key\n"),
        }])
        .unwrap();

        assert_eq!(&archive[..6], b"070701", "magic: 070702 would demand a real chksum");
        assert_eq!(field(&archive, 0, 0), 721, "ino");
        assert_eq!(field(&archive, 0, 1), 0o100644, "mode: S_IFREG, not a symlink or a device");
        assert_eq!(field(&archive, 0, 2), 0, "uid");
        assert_eq!(field(&archive, 0, 3), 0, "gid");
        assert_eq!(field(&archive, 0, 4), 1, "nlink");
        assert_eq!(field(&archive, 0, 5), 1, "mtime: `gen_init_cpio -t 1`");
        assert_eq!(field(&archive, 0, 6), 4, "filesize");
        assert_eq!(field(&archive, 0, 7), 3, "devmajor");
        assert_eq!(field(&archive, 0, 8), 1, "devminor");
        assert_eq!(field(&archive, 0, 9), 0, "rdevmajor");
        assert_eq!(field(&archive, 0, 10), 0, "rdevminor");
        assert_eq!(field(&archive, 0, 11), 22, "namesize counts the NUL");
        assert_eq!(field(&archive, 0, 12), 0, "chksum");
        assert_eq!(name_at(&archive, 0, 22), "etc/td/deployment.pub");
    }

    /// `nlink = 1` is what keeps the kernel's hardlink path out of this
    /// entirely, which is the whole reason an inode collision with the base
    /// archive is safe. It is asserted separately from the offsets because it
    /// is a behavioural claim rather than a layout one.
    #[test]
    fn a_regular_file_claims_exactly_one_link() {
        let archive = build(&[Entry {
            name: "k",
            mode: 0o600,
            kind: Kind::File(b"x"),
        }])
        .unwrap();
        assert_eq!(field(&archive, 0, 4), 1);
    }

    #[test]
    fn a_directory_is_written_with_the_type_bits_and_link_count_gen_init_cpio_uses() {
        let archive = build(&[Entry {
            name: "etc",
            mode: 0o755,
            kind: Kind::Directory,
        }])
        .unwrap();
        assert_eq!(field(&archive, 0, 1), 0o040755, "mode: S_IFDIR, not a block device");
        assert_eq!(field(&archive, 0, 4), 2, "nlink");
        assert_eq!(field(&archive, 0, 6), 0, "filesize");
    }

    /// Every header must start 4-aligned AND every padding byte must be NUL —
    /// the kernel's restart branch is `*buf == '0' && !(this_header & 3)`
    /// (`init/initramfs.c:533`), and what carries it across a gap is the
    /// one-byte-at-a-time NUL skip below it. A 0xFF pad is neither, so it stops
    /// the walk exactly as a misaligned header does.
    ///
    /// Walking the archive is how both are checked, since the padding after a
    /// name and the padding after data are two separate rules and an archive
    /// with one of them wrong still starts correctly.
    #[test]
    fn every_header_starts_four_aligned_and_every_pad_byte_is_nul() {
        let archive = build(&[
            Entry { name: "etc", mode: 0o755, kind: Kind::Directory },
            // Deliberately awkward: a name and a body whose lengths are not
            // multiples of four, so both padding rules have to be right.
            Entry { name: "etc/a", mode: 0o644, kind: Kind::File(b"12345") },
            Entry { name: "etc/bb", mode: 0o644, kind: Kind::File(b"1") },
            Entry { name: "etc/ccc", mode: 0o600, kind: Kind::File(&[]) },
        ])
        .unwrap();

        let mut at = 0usize;
        let mut seen = 0;
        loop {
            assert_eq!(at % 4, 0, "header {seen} starts at {at}, which is not 4-aligned");
            assert_eq!(&archive[at..at + 6], b"070701", "header {seen} magic");
            let namesize = field(&archive, at, 11) as usize;
            let filesize = field(&archive, at, 6) as usize;
            let name = name_at(&archive, at, namesize);
            let after_name = at + HEADER_LEN + namesize;
            let padded_name = after_name.next_multiple_of(4);
            let after_data = padded_name + filesize;
            let next = after_data.next_multiple_of(4);
            for (label, range) in
                [("name", after_name..padded_name), ("data", after_data..next)]
            {
                for offset in range {
                    assert_eq!(
                        archive.get(offset).copied(),
                        Some(0),
                        "the {label} padding of header {seen} is not NUL at {offset}"
                    );
                }
            }
            at = next;
            seen += 1;
            if name == TRAILER_NAME {
                break;
            }
            assert!(seen < 16, "walked off the end without finding a trailer");
        }
        assert_eq!(seen, 5, "four entries and the trailer");
        assert_eq!(at, archive.len(), "the walk must consume the archive exactly");
    }

    /// The trailer's device numbers are 0 where a real entry's are 3 and 1
    /// (`usr/gen_init_cpio.c:87-118` against `:436-451`), which is easy to get
    /// wrong by writing one header routine and reusing its constants.
    #[test]
    fn the_trailer_matches_gen_init_cpios_trailer() {
        let archive = build(&[]).unwrap();
        // 110 header + 11 name-with-NUL = 121, padded to 4 = 124. Spelled as a
        // literal rather than recomputed from the code's own formula, which
        // would agree with itself however wrong it was.
        assert_eq!(archive.len(), 124, "one header, its name, and padding to 4");
        // EVERY field, not a selection: the two divergences found in review were
        // both fields this test had skipped, and the reason it skipped them is
        // that they are the ones a header routine shared with real entries gets
        // wrong. So the loop asserts zero everywhere and the two exceptions are
        // named.
        for index in 0..13 {
            let expected = match index {
                4 => 1,  // nlink
                11 => 11, // namesize, counting the NUL
                _ => 0,
            };
            assert_eq!(
                field(&archive, 0, index),
                expected,
                "trailer field {index} (mtime is 5, devmajor 7, devminor 8 — all zero here \
                 where a real entry has 1, 3 and 1)"
            );
        }
        assert_eq!(name_at(&archive, 0, 11), TRAILER_NAME);
        // And the contrast that makes those three meaningful.
        let entry = build(&[Entry { name: "x", mode: 0o644, kind: Kind::File(b"y") }]).unwrap();
        assert_eq!(field(&entry, 0, 5), 1, "a real entry's mtime is not the trailer's");
        assert_eq!(field(&entry, 0, 7), 3, "devmajor");
        assert_eq!(field(&entry, 0, 8), 1, "devminor");
    }

    /// `alignment_padding` returns a COUNT, so NUL-ness is the caller's to get
    /// right and is asserted where the bytes exist, in the archive walk above.
    /// What is checked here is the arithmetic alone.
    #[test]
    fn alignment_padding_is_the_least_that_lands_the_next_magic_on_four() {
        for length in 0..16usize {
            let padding = alignment_padding(length);
            assert!(padding < 4, "length {length} asked for {padding} bytes of padding");
            assert_eq!((length + padding) % 4, 0, "length {length}");
        }
    }

    /// The same entries must give the same bytes: this archive's content
    /// changes the initramfs hash, which the manifest names, whose digest is the
    /// deployment id.
    #[test]
    fn building_the_same_entries_twice_gives_the_same_bytes() {
        let entries = || {
            vec![
                Entry { name: "etc", mode: 0o755, kind: Kind::Directory },
                Entry { name: "etc/deployment.pub", mode: 0o644, kind: Kind::File(b"abc\n") },
            ]
        };
        assert_eq!(build(&entries()).unwrap(), build(&entries()).unwrap());
    }

    /// Each case is pinned to the REASON it is refused, not merely to being
    /// refused. Several of these rules overlap — `/etc/x` is both absolute and
    /// has an empty first component — so an `is_err()` sweep passes with any one
    /// of them deleted, which a verify-red probe caught it doing for the
    /// absolute-name rule specifically. The message is also the diagnostic a
    /// caller gets, and "is absolute" is worth more than "empty component".
    #[test]
    fn names_that_would_place_a_file_somewhere_else_are_refused_for_the_stated_reason() {
        for (name, reason) in [
            ("", "may not be empty"),
            ("/etc/x", "is absolute"),
            ("../escape", ".."),
            ("etc/../../escape", ".."),
            ("etc/./x", "\".\""),
            ("etc//x", "empty component"),
            ("TRAILER!!!", "end-of-archive marker"),
            ("café", "not ASCII"),
            // Well-formed for the format; it is the recipes' `grep -x -F`
            // membership assertions a newline would quietly defeat.
            ("etc/a\nb", "control byte 0x0a"),
            ("etc/a\x7fb", "control byte 0x7f"),
            // PATH_MAX counts the NUL, so 4096 name bytes is one too many. This
            // is the boundary that matters: the kernel SKIPS such an entry
            // without a word, so a writer that accepted it would report success
            // and install nothing.
            (&"a".repeat(PATH_MAX), "over PATH_MAX"),
        ] {
            let error = build(&[Entry { name, mode: 0o644, kind: Kind::File(b"x") }])
                .expect_err(&format!("{name:?} must be refused"));
            assert!(
                error.contains(reason),
                "{name:?} must be refused for {reason:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn a_mode_carrying_type_bits_is_refused_rather_than_masked() {
        assert!(build(&[Entry {
            name: "x",
            mode: S_IFREG | 0o644,
            kind: Kind::File(b"x"),
        }])
        .is_err());
        assert!(build(&[Entry { name: "x", mode: 0o7777, kind: Kind::Directory }]).is_ok());
    }

    /// The other side of the PATH_MAX boundary, so the refusal is a boundary
    /// rather than a blanket one: a name of exactly `PATH_MAX - 1` bytes has a
    /// `namesize` of exactly `PATH_MAX` and is the longest the kernel accepts.
    #[test]
    fn the_longest_name_the_kernel_accepts_is_accepted() {
        let name = "a".repeat(PATH_MAX - 1);
        let archive = build(&[Entry {
            name: &name,
            mode: 0o644,
            kind: Kind::File(b"x"),
        }])
        .expect("a name of PATH_MAX - 1 bytes is within the bound");
        assert_eq!(
            field(&archive, 0, 11) as usize,
            PATH_MAX,
            "namesize is exactly PATH_MAX at the boundary"
        );
    }

    /// All sixteen digits, so the whole table is pinned rather than the eleven
    /// a `DEADBEEF`/`FFFFFFFF` pair happens to reach. `hex8` falls back to `'0'`
    /// on a lookup miss, which cannot happen through a `& 0xf` index — but the
    /// fallback is silent-wrong-output rather than a failure, so what keeps it
    /// unreachable-in-practice is this covering every entry it could miss.
    #[test]
    fn hex8_is_eight_uppercase_digits() {
        assert_eq!(&hex8(0), b"00000000");
        assert_eq!(&hex8(1), b"00000001");
        assert_eq!(&hex8(0x0123_4567), b"01234567");
        assert_eq!(&hex8(0x89ab_cdef), b"89ABCDEF");
        assert_eq!(&hex8(0xdead_beef), b"DEADBEEF");
        assert_eq!(&hex8(u32::MAX), b"FFFFFFFF");
        assert_eq!(&hex8(FIRST_INO), b"000002D1");
    }
}
