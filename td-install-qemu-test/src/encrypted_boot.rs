//! The guest legs of `qemu-boot-encrypted` (td-install/ENCRYPTION.md
//! increment 6) that need a key the installed system never shows: the
//! recovery key, which the host carries in this leg's initramfs only, and
//! the volume key, which this process takes from cryptsetup into RAM to
//! search for it and never prints. Every copy here is zeroed when dropped.

use super::encrypted::{
    contains, cryptsetup, drop_page_cache, partitions, require_absent, Needles, CRYPTSETUP,
    INACTIVE, MAPPING,
};
use super::*;
use installation_protocol::{scrub, RecoveryDigits, RECOVERY_DIGITS};

/// The private port the installation leg sends the key typed back over:
/// the virtio serial port the host names `ORACLE_SIDE_PORT`, found by
/// that name, since nothing here makes the `/dev/virtio-ports` links.
pub(super) fn key_channel() -> Result<PathBuf, String> {
    let class = Path::new("/sys/class/virtio-ports");
    let entries =
        fs::read_dir(class).map_err(|error| format!("list {}: {error}", class.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("list {}: {error}", class.display()))?;
        let name = read(&entry.path().join("name"), 256)?;
        if name.trim_ascii() == ORACLE_SIDE_PORT.as_bytes() {
            return Ok(Path::new("/dev").join(entry.file_name()));
        }
    }
    Err(format!("no virtio port is named {ORACLE_SIDE_PORT}"))
}
/// The volume key's length: AES-XTS with a 512-bit key.
const VOLUME_KEY_BYTES: usize = 64;
/// A RAM directory of the initramfs root for cryptsetup's key file.
const KEY_DIRECTORY: &str = "/oracle-volume-key";
/// The most bytes of the host's artifacts this leg is given.
const MAX_ARTIFACTS: u64 = 64 << 20;
/// The keyslots the header leg adds: an orphan with no token, the
/// superseded token's own, and the one whose kill leaves its token an
/// orphan. Free after the first-boot transition, which leaves keyslots 0
/// and 2.
const ORPHAN_SLOT: &str = "1";
const SUPERSEDED_SLOT: &str = "3";
const ORPHANED_TOKEN_SLOT: &str = "4";
/// The token numbers the header leg imports at: free beside the one
/// device-bound token the transition left at 1.
const SUPERSEDED_TOKEN: &str = "2";
const ORPHANED_TOKEN: &str = "0";

/// The recovery key the host placed in this leg's initramfs, removed from
/// the root once read.
fn recovery_key() -> Result<RecoveryDigits, String> {
    let path = Path::new("/").join(ORACLE_RECOVERY_KEY);
    let mut bytes = read(&path, RECOVERY_DIGITS as u64)?;
    let key = RecoveryDigits::new(&bytes);
    scrub(&mut bytes);
    fs::remove_file(&path).map_err(|error| format!("remove {}: {error}", path.display()))?;
    key
}

/// The system root and its store, for the verified root's cryptsetup.
fn mount_tools(device: &str) -> Result<(), String> {
    mount_source(device)?;
    mount_system_root()?;
    bind_root_store()
}

/// cryptsetup with `input` on standard input and an empty environment;
/// its exit code and standard output, which never reaches the console.
fn cryptsetup_output(args: &[&str], input: &[u8]) -> Result<(Option<i32>, Vec<u8>), String> {
    let mut child = Command::new(CRYPTSETUP)
        .args(args)
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("start cryptsetup {}: {error}", args.join(" ")))?;
    let fed = child
        .stdin
        .take()
        .ok_or("cryptsetup stdin is unavailable")
        .and_then(|mut stdin| stdin.write_all(input).map_err(|_| "feed cryptsetup"));
    let mut output = Vec::new();
    let read = child
        .stdout
        .take()
        .ok_or("cryptsetup stdout is unavailable")
        .and_then(|stdout| {
            stdout
                .take(128 * 1024)
                .read_to_end(&mut output)
                .map_err(|_| "read cryptsetup")
        });
    let status = child
        .wait()
        .map_err(|error| format!("wait for cryptsetup: {error}"))?;
    fed?;
    read?;
    Ok((status.code(), output))
}

fn require_success(what: &str, code: Option<i32>) -> Result<(), String> {
    match code {
        Some(0) => Ok(()),
        other => Err(format!("cryptsetup {what} failed: {other:?}")),
    }
}

/// The volume key, held in one heap buffer zeroed on drop.
struct VolumeKey(Box<[u8; VOLUME_KEY_BYTES]>);

impl Drop for VolumeKey {
    fn drop(&mut self) {
        scrub(&mut self.0[..]);
    }
}

/// The volume key from `luksDump --dump-volume-key`, which writes it to a
/// file it creates in a fresh mode-0700 directory of the RAM-backed root;
/// the file is read and unlinked at once, and the directory removed,
/// whatever happened.
fn volume_key(volume: &str, key: &RecoveryDigits) -> Result<VolumeKey, String> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(KEY_DIRECTORY)
        .map_err(|error| format!("create {KEY_DIRECTORY}: {error}"))?;
    let file = Path::new(KEY_DIRECTORY).join("key");
    let name = file.to_str().ok_or("non-UTF-8 key path")?;
    let dumped = cryptsetup_output(
        &[
            "luksDump",
            "--dump-volume-key",
            "--batch-mode",
            "--volume-key-file",
            name,
            "--key-file=-",
            volume,
        ],
        key.as_bytes(),
    )
    .and_then(|(code, _)| require_success("luksDump --dump-volume-key", code));
    let mut held = VolumeKey(Box::new([0; VOLUME_KEY_BYTES]));
    let read = dumped.and_then(|()| {
        let mut opened =
            File::open(&file).map_err(|error| format!("open the volume key: {error}"))?;
        opened
            .read_exact(&mut held.0[..])
            .map_err(|error| format!("read the volume key: {error}"))?;
        let mut extra = [0u8; 1];
        match opened.read(&mut extra) {
            Ok(0) => Ok(()),
            _ => Err("the volume key file is not 64 bytes".to_string()),
        }
    });
    let unlinked = match fs::remove_file(&file) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("unlink the volume key: {error}"))
        }
        _ => Ok(()),
    };
    let removed =
        fs::remove_dir(KEY_DIRECTORY).map_err(|error| format!("remove {KEY_DIRECTORY}: {error}"));
    read?;
    unlinked?;
    removed?;
    Ok(held)
}

/// Lowercase or uppercase hexadecimal of `bytes`.
fn hex(bytes: &[u8], upper: bool) -> Vec<u8> {
    let digits: &[u8; 16] = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut text = Vec::with_capacity(bytes.len() * 2);
    for byte in bytes {
        for nibble in [byte >> 4, byte & 0xf] {
            text.push(digits.get(usize::from(nibble)).copied().unwrap_or(b'0'));
        }
    }
    text
}

/// What the inspection searches for: the volume key raw and in both
/// cases of hexadecimal, and the recovery key's two forms.
struct Secrets {
    raw: VolumeKey,
    lower: Vec<u8>,
    upper: Vec<u8>,
    recovery: Needles,
}

impl Drop for Secrets {
    fn drop(&mut self) {
        scrub(&mut self.lower);
        scrub(&mut self.upper);
    }
}

impl Secrets {
    /// The longest form, less one: the overlap between reads.
    const KEEP: usize = 2 * VOLUME_KEY_BYTES - 1;

    fn found(&self, window: &[u8]) -> Option<&'static str> {
        if contains(window, &self.raw.0[..]) {
            Some("the volume key")
        } else if contains(window, &self.lower) || contains(window, &self.upper) {
            Some("the volume key in hexadecimal")
        } else if self.recovery.found_in(window) {
            Some("the recovery key")
        } else {
            None
        }
    }
}

/// The inspection leg, after every boot of the installed disk: with the
/// recovery key the host carried here, the volume key taken into RAM; then
/// neither key on the whole disk (read past the page cache), in the opened
/// volume's plaintext, logs included, or in the host's saved consoles and
/// logs. The mapping is opened read-only and closed, as device-mapper
/// answers, whatever happened.
pub(super) fn inspect(device: &str) -> Result<(), String> {
    mount_tools(device)?;
    let key = recovery_key()?;
    let (_, volume) = partitions(device)?;
    let raw = volume_key(&volume, &key)?;
    let secrets = Secrets {
        lower: hex(&raw.0[..], false),
        upper: hex(&raw.0[..], true),
        raw,
        recovery: Needles::new(&key),
    };
    drop_page_cache()?;
    let found = |window: &[u8]| secrets.found(window);
    let disk = require_absent(device, Secrets::KEEP, &found)?;
    match cryptsetup(
        &[
            "open",
            "--readonly",
            "--type",
            "luks2",
            "--key-file=-",
            &volume,
            MAPPING,
        ],
        key.as_bytes(),
    )? {
        Some(0) => {}
        other => {
            return Err(format!(
                "the recovery key does not open {volume}: {other:?}"
            ))
        }
    }
    let mapped = require_absent(&format!("/dev/mapper/{MAPPING}"), Secrets::KEEP, &found);
    let closed = cryptsetup(&["close", MAPPING], &[])
        .and_then(|_| cryptsetup(&["status", MAPPING], &[]))
        .and_then(|code| match code {
            Some(INACTIVE) => Ok(()),
            other => Err(format!(
                "the mapping {MAPPING} survives its close: {other:?}"
            )),
        });
    let mapped = mapped?;
    closed?;
    // A leg's disk copy is inspected with no artifacts: the host then
    // expects 0 searched.
    let artifacts = Path::new("/").join(ORACLE_ARTIFACTS);
    let searched = match fs::metadata(&artifacts) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => return Err(format!("stat {}: {error}", artifacts.display())),
        Ok(metadata) => {
            let length = metadata.len();
            if length == 0 || length > MAX_ARTIFACTS {
                return Err(format!("the host's artifacts hold {length} bytes"));
            }
            require_absent(
                artifacts.to_str().ok_or("non-UTF-8 artifacts path")?,
                Secrets::KEEP,
                &found,
            )?
        }
    };
    drop(secrets);
    drop(key);
    command("/bin/umount", &["/td/store"])?;
    report(
        std::io::stdout(),
        format_args!("{ENCRYPTED_SECRETS_ABSENT_MARKER} {disk} {mapped} {searched}"),
    )?;
    report(std::io::stdout(), format_args!("{ENCRYPTED_END_MARKER}"))
}

/// The value of the string field `name` in a token's JSON, whitespace
/// already removed: td's hexadecimal fields hold no quote or escape.
fn string_field<'a>(json: &'a str, name: &str) -> Result<&'a str, String> {
    let lead = format!("\"{name}\":\"");
    let start = json
        .find(&lead)
        .map(|at| at + lead.len())
        .ok_or_else(|| format!("the token has no {name}"))?;
    let rest = json.get(start..).ok_or("short token")?;
    let end = rest.find('"').ok_or("unterminated token field")?;
    rest.get(..end).ok_or_else(|| "short token".to_string())
}

/// The one td-protector token on `volume`, its number and JSON without
/// whitespace; there must be exactly one, device-bound.
fn device_bound_token(volume: &str) -> Result<(u8, String), String> {
    let mut found = Vec::new();
    for number in 0..32u8 {
        let id = number.to_string();
        let (code, output) =
            cryptsetup_output(&["token", "export", "--token-id", &id, volume], &[])?;
        if code != Some(0) {
            continue;
        }
        let json: String = String::from_utf8(output)
            .map_err(|_| "non-UTF-8 token JSON")?
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        if json.contains("\"type\":\"td-protector\"") {
            found.push((number, json));
        }
    }
    match found.as_slice() {
        [(number, json)] if json.contains("\"role\":\"device-bound\"") => {
            Ok((*number, json.clone()))
        }
        _ => Err(format!(
            "{volume} carries {} td tokens, not one device-bound one",
            found.len()
        )),
    }
}

/// A random passphrase for a keyslot the header leg adds: 32 bytes of
/// `/dev/urandom` in hexadecimal, in a mode-0600 file of the RAM root.
fn throwaway_key(name: &str) -> Result<PathBuf, String> {
    let mut random = [0u8; 32];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut random))
        .map_err(|error| format!("read /dev/urandom: {error}"))?;
    let path = Path::new("/").join(name);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut file| file.write_all(&hex(&random, false)))
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok(path)
}

/// One keyslot added with a throwaway passphrase, keyslot 0's recovery key
/// authorizing it, with the format's PBKDF2 parameters.
fn add_keyslot(volume: &str, slot: &str, key: &RecoveryDigits) -> Result<(), String> {
    let file = throwaway_key(&format!("oracle-keyslot-{slot}"))?;
    let name = file.to_str().ok_or("non-UTF-8 key path")?;
    let added = cryptsetup(
        &[
            "luksAddKey",
            "--batch-mode",
            "--pbkdf",
            "pbkdf2",
            "--pbkdf-force-iterations",
            "1000",
            "--hash",
            "sha256",
            "--key-slot",
            "0",
            "--new-key-slot",
            slot,
            "--key-file=-",
            volume,
            name,
        ],
        key.as_bytes(),
    );
    let removed = fs::remove_file(&file).map_err(|error| format!("remove {name}: {error}"));
    require_success(&format!("luksAddKey of keyslot {slot}"), added?)?;
    removed
}

fn import_token(volume: &str, number: &str, json: &str) -> Result<(), String> {
    let code = cryptsetup(
        &[
            "token",
            "import",
            "--token-id",
            number,
            "--json-file=-",
            volume,
        ],
        json.as_bytes(),
    )?;
    require_success(&format!("token import at {number}"), code)
}

/// The header-state leg, on a volume whose first-boot transition finished:
/// with cryptsetup and the recovery key, an orphan keyslot with no token,
/// a superseded device-bound token (the transition's sealed object again)
/// on a keyslot of its own, and a first-boot token whose keyslot was then
/// killed, which cryptsetup leaves naming none. The installed selector's
/// next plan must retire all three.
pub(super) fn build_headers(device: &str) -> Result<(), String> {
    mount_tools(device)?;
    let key = recovery_key()?;
    let (_, volume) = partitions(device)?;
    let (number, json) = device_bound_token(&volume)?;
    if number.to_string() == SUPERSEDED_TOKEN || number.to_string() == ORPHANED_TOKEN {
        return Err(format!("the device-bound token is at {number}"));
    }
    let public = string_field(&json, "public")?;
    let private = string_field(&json, "private")?;
    let token = |slot: &str, role: &str| {
        format!(
            "{{\"type\":\"td-protector\",\"keyslots\":[\"{slot}\"],\"role\":\"{role}\",\
             \"public\":\"{public}\",\"private\":\"{private}\"}}"
        )
    };
    add_keyslot(&volume, ORPHAN_SLOT, &key)?;
    add_keyslot(&volume, SUPERSEDED_SLOT, &key)?;
    import_token(
        &volume,
        SUPERSEDED_TOKEN,
        &token(SUPERSEDED_SLOT, "device-bound"),
    )?;
    add_keyslot(&volume, ORPHANED_TOKEN_SLOT, &key)?;
    import_token(
        &volume,
        ORPHANED_TOKEN,
        &token(ORPHANED_TOKEN_SLOT, "first-boot"),
    )?;
    let killed = cryptsetup(
        &[
            "luksKillSlot",
            "--batch-mode",
            "--key-file=-",
            &volume,
            ORPHANED_TOKEN_SLOT,
        ],
        key.as_bytes(),
    )?;
    require_success(&format!("luksKillSlot {ORPHANED_TOKEN_SLOT}"), killed)?;
    drop(key);
    applet(&["sync"])?;
    command("/bin/umount", &["/td/store"])?;
    report(
        std::io::stdout(),
        format_args!(
            "{ENCRYPTED_HEADERS_MARKER} orphan-keyslot {ORPHAN_SLOT} superseded-token \
             {SUPERSEDED_TOKEN} on {SUPERSEDED_SLOT} orphaned-token {ORPHANED_TOKEN} \
             device-bound-token {number}"
        ),
    )?;
    report(std::io::stdout(), format_args!("{ENCRYPTED_END_MARKER}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    #[test]
    fn hex_is_both_cases_of_each_nibble() {
        assert_eq!(hex(&[0x00, 0x9f, 0xa5, 0xff], false), b"009fa5ff");
        assert_eq!(hex(&[0x00, 0x9f, 0xa5, 0xff], true), b"009FA5FF");
    }

    #[test]
    fn a_token_field_is_read_to_its_closing_quote() {
        let json = "{\"type\":\"td-protector\",\"keyslots\":[\"2\"],\"role\":\"device-bound\",\"public\":\"00ab\",\"private\":\"ff\"}";
        assert_eq!(string_field(json, "public").unwrap(), "00ab");
        assert_eq!(string_field(json, "private").unwrap(), "ff");
        assert_eq!(string_field(json, "role").unwrap(), "device-bound");
        assert!(string_field(json, "absent").is_err());
        assert!(string_field("{\"public\":\"00", "public").is_err());
    }

    #[test]
    fn the_secrets_name_each_form_and_the_overlap_fits_the_longest() {
        let key = RecoveryDigits::new(b"123456789012345678901234567890123456789012345678").unwrap();
        let mut raw = Box::new([0u8; VOLUME_KEY_BYTES]);
        for (index, byte) in raw.iter_mut().enumerate() {
            *byte = index as u8 ^ 0x5a;
        }
        let secrets = Secrets {
            lower: hex(&raw[..], false),
            upper: hex(&raw[..], true),
            raw: VolumeKey(raw.clone()),
            recovery: Needles::new(&key),
        };
        assert_eq!(Secrets::KEEP + 1, secrets.lower.len());
        assert_eq!(secrets.found(b"nothing here"), None);
        let mut window = b"x".to_vec();
        window.extend_from_slice(&raw[..]);
        assert_eq!(secrets.found(&window), Some("the volume key"));
        assert_eq!(
            secrets.found(&secrets.upper.clone()),
            Some("the volume key in hexadecimal")
        );
        assert_eq!(
            secrets.found(b"123456789012345678901234567890123456789012345678"),
            Some("the recovery key")
        );
        assert_eq!(secrets.found(&raw[1..]), None);
    }
}
