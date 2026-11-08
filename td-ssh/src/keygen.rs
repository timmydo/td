//! `ssh-keygen`'s subset td uses: generation, public-half and fingerprint
//! reporting, and SSHSIG signatures (DESIGN.md section 6).

use crate::error::{diagnostic, peer_text, primary_output, Error, Result};
use crate::kex::{ed25519_public_blob, parse_ed25519_public, HOST_KEY_ALGORITHM};
use crate::keyfile::{wipe_text, PrivateKey, PrivateKeyFile, PublicKey};
use crate::prompt::Prompter;
use crate::wire::{Reader, Writer};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use td_encoding::{base64, from_base64};
use td_primitives::curve25519::ed25519;
use td_primitives::sha::{Digest, Sha512};

const SIG_MAGIC: &[u8] = b"SSHSIG";
const SIG_VERSION: u32 = 1;
const SIG_HASH: &str = "sha512";
const SIG_BEGIN: &str = "-----BEGIN SSH SIGNATURE-----";
const SIG_END: &str = "-----END SSH SIGNATURE-----";
const MAX_FILE: u64 = 64 * 1024;
const USAGE: &str = "usage: ssh-keygen [-q] -t ed25519 -N '' [-C comment] [-f file]\n       ssh-keygen -y [-P pass] -f file\n       ssh-keygen -l [-E sha256] -f file\n       ssh-keygen -Y sign -n namespace -f key [file]\n       ssh-keygen -Y verify -n namespace -I identity -f allowed -s signature";

#[derive(Default, Debug)]
struct Arguments {
    generate_type: Option<String>,
    new_passphrase: Option<String>,
    /// `-P`, for `-y` alone: td-firstboot and td-vm-guest pass `-P ''` so that
    /// a protected key fails rather than prompting on a closed stdin.
    old_passphrase: Option<String>,
    comment: Option<String>,
    file: Option<PathBuf>,
    print_public: bool,
    fingerprint: bool,
    hash_name: Option<String>,
    quiet: bool,
    sign_action: Option<String>,
    namespace: Option<String>,
    identity: Option<String>,
    signature: Option<PathBuf>,
    /// File names, which like `-f` and `-s` may be any bytes.
    positional: Vec<PathBuf>,
}

const VALUE_FLAGS: &[u8] = b"tNPCfEYnIs";

fn parse(args: &[OsString]) -> Result<Arguments> {
    let mut parsed = Arguments::default();
    let mut index = 0usize;
    while let Some(arg) = args.get(index) {
        index += 1;
        let bytes = arg.as_bytes();
        if !bytes.starts_with(b"-") || bytes == b"-" {
            parsed.positional.push(PathBuf::from(arg));
            continue;
        }
        for (at, flag) in bytes.iter().copied().enumerate().skip(1) {
            if VALUE_FLAGS.contains(&flag) {
                let attached = bytes.get(at + 1..).unwrap_or_default();
                let value = if attached.is_empty() {
                    let value = args.get(index).ok_or_else(|| {
                        Error::refused(format!("option -{} needs a value", flag.escape_ascii()))
                    })?;
                    index += 1;
                    value.as_os_str()
                } else {
                    OsStr::from_bytes(attached)
                };
                let text = || {
                    value.to_str().map(str::to_string).ok_or_else(|| {
                        Error::refused(format!("option -{}: not UTF-8", flag.escape_ascii()))
                    })
                };
                match flag {
                    b't' => parsed.generate_type = Some(text()?),
                    b'N' => parsed.new_passphrase = Some(text()?),
                    b'P' => parsed.old_passphrase = Some(text()?),
                    b'C' => parsed.comment = Some(text()?),
                    b'f' => parsed.file = Some(PathBuf::from(value)),
                    b'E' => parsed.hash_name = Some(text()?),
                    b'Y' => parsed.sign_action = Some(text()?),
                    b'n' => parsed.namespace = Some(text()?),
                    b'I' => parsed.identity = Some(text()?),
                    _ => parsed.signature = Some(PathBuf::from(value)),
                }
                break;
            }
            match flag {
                b'y' => parsed.print_public = true,
                b'l' => parsed.fingerprint = true,
                b'q' => parsed.quiet = true,
                other => {
                    return Err(Error::refused(format!(
                        "unknown option -- {}\n{USAGE}",
                        other.escape_ascii()
                    )))
                }
            }
        }
    }
    Ok(parsed)
}

fn read_bounded(path: &Path) -> Result<String> {
    let metadata = fs::metadata(path)
        .map_err(|error| Error::refused(format!("{}: {error}", path.display())))?;
    if metadata.len() > MAX_FILE {
        return Err(Error::refused(format!("{}: too large", path.display())));
    }
    fs::read_to_string(path).map_err(|error| Error::refused(format!("{}: {error}", path.display())))
}

fn required_file(arguments: &Arguments) -> Result<&Path> {
    arguments
        .file
        .as_deref()
        .ok_or_else(|| Error::refused(USAGE))
}

/// `/dev/random`: early in boot, a host key waits for the kernel's CRNG to be
/// seeded, and after that never.
fn random(out: &mut [u8]) -> Result<()> {
    File::open("/dev/random")
        .and_then(|mut file| file.read_exact(out))
        .map_err(|error| Error::refused(format!("reading entropy: {error}")))
}

/// Created exclusively, so nothing else can be holding the path open. What this
/// call created, it removes again if it cannot finish writing.
fn write_new(path: &Path, text: &str, mode: u32) -> Result<()> {
    let refuse = |error: io::Error| Error::refused(format!("{}: {error}", path.display()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(refuse)?;
    let written = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all());
    if written.is_err() {
        let _ = fs::remove_file(path);
    }
    written.map_err(refuse)
}

/// Private then public, so no public half ever names a key that is not there,
/// and a public half that cannot be written takes the private one with it.
fn write_pair(path: &Path, private: &str, public_path: &Path, public: &str) -> Result<()> {
    write_new(path, private, 0o600)?;
    let written = write_new(public_path, public, 0o644);
    if written.is_err() {
        let _ = fs::remove_file(path);
    }
    written
}

/// Refuses before any secret is made, with both targets checked first, so a
/// failure leaves no partial pair.
fn generate(arguments: &Arguments) -> Result<i32> {
    // Stock would prompt without `-N`. Here that would be a promise of protection
    // this tool cannot keep, so an unprotected key is asked for by name.
    if arguments.new_passphrase.as_deref() != Some("") {
        return Err(Error::refused(
            "td-ssh-keygen writes only unprotected keys: say so with -N ''",
        ));
    }
    let key_type = arguments.generate_type.as_deref().unwrap_or("ed25519");
    if key_type != "ed25519" {
        return Err(Error::refused(format!(
            "unknown key type {key_type}: td-ssh-keygen only generates ed25519"
        )));
    }
    let path = match &arguments.file {
        Some(path) => path.clone(),
        None => {
            let path = default_key_path()?;
            if let Some(created) = create_default_directory(&path)? {
                if !arguments.quiet {
                    primary_output(&format!("Created directory '{}'.\n", created.display()))?;
                }
            }
            path
        }
    };
    let public_path = public_path_of(&path);
    for target in [&path, &public_path] {
        // No following: a dangling symlink is a name `create_new` would refuse.
        if fs::symlink_metadata(target).is_ok() {
            return Err(Error::refused(format!(
                "{} already exists.",
                target.display()
            )));
        }
    }
    let mut seed = [0u8; 32];
    random(&mut seed)?;
    let mut check = [0u8; 4];
    random(&mut check)?;
    let comment = arguments.comment.clone().unwrap_or_else(default_comment);
    let key = PrivateKey::from_seed(&seed, &comment);
    td_primitives::ct::zero(&mut seed);
    let key = key?;
    let text = key.to_file(u32::from_be_bytes(check));
    let public = key.public();
    let written = write_pair(&path, &text, &public_path, &public.to_line());
    wipe_text(text);
    written?;
    if !arguments.quiet {
        primary_output(&format!(
            "Your identification has been saved in {}\nYour public key has been saved in {}\nThe key fingerprint is:\n{} {comment}\n",
            path.display(),
            public_path.display(),
            public.fingerprint()
        ))?;
    }
    Ok(0)
}

fn default_key_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or_else(|| Error::refused("no HOME for the default key path; use -f"))?;
    Ok(PathBuf::from(home).join(".ssh/id_ed25519"))
}

/// A key at the default path has `~/.ssh` made for it, private, as OpenSSH does.
/// A path the caller named is theirs to have prepared. The directory, if made.
fn create_default_directory(path: &Path) -> Result<Option<PathBuf>> {
    let Some(directory) = path.parent() else {
        return Ok(None);
    };
    match fs::DirBuilder::new().mode(0o700).create(directory) {
        Ok(()) => Ok(Some(directory.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn default_comment() -> String {
    let user = std::env::var("USER").unwrap_or_default();
    let host = fs::read_to_string("/etc/hostname").unwrap_or_default();
    format!("{user}@{}", host.trim())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn public_path_of(path: &Path) -> PathBuf {
    with_suffix(path, ".pub")
}

fn load_private(path: &Path, passphrase: Option<&str>, batch_prompt: bool) -> Result<PrivateKey> {
    let text = read_bounded(path)?;
    let file = PrivateKeyFile::parse(&text);
    wipe_text(text);
    let file = file?;
    if !file.is_encrypted() {
        return file.decrypt(None);
    }
    if let Some(passphrase) = passphrase {
        return file.decrypt(Some(passphrase.as_bytes()));
    }
    let mut answer = Prompter::new(batch_prompt)
        .secret(&format!("Enter passphrase for \"{}\": ", path.display()))?;
    let key = file.decrypt(Some(&answer));
    td_primitives::ct::zero(&mut answer);
    key
}

fn print_public(arguments: &Arguments) -> Result<i32> {
    let key = load_private(
        required_file(arguments)?,
        arguments.old_passphrase.as_deref(),
        false,
    )?;
    primary_output(&key.public().to_line())?;
    Ok(0)
}

/// Fingerprints a public line, or a private file's cleartext public half.
fn fingerprint(arguments: &Arguments) -> Result<i32> {
    if let Some(hash) = arguments.hash_name.as_deref() {
        if hash != "sha256" {
            return Err(Error::refused(format!(
                "td-ssh-keygen only prints sha256 fingerprints, not {hash}"
            )));
        }
    }
    let path = required_file(arguments)?;
    let text = read_bounded(path)?;
    let key = public_half(&text, path);
    wipe_text(text);
    let key = key?;
    primary_output(&fingerprint_line(&key))?;
    Ok(0)
}

/// A key file's comment came from whoever made the file, so it goes through the
/// filter on its way to a terminal.
fn fingerprint_line(key: &PublicKey) -> String {
    let comment = if key.comment.is_empty() {
        "no comment".to_string()
    } else {
        peer_text(key.comment.as_bytes())
    };
    format!("256 {} {comment} (ED25519)\n", key.fingerprint())
}

fn public_half(text: &str, path: &Path) -> Result<PublicKey> {
    if let Ok(key) = PublicKey::parse_line(text.lines().next().unwrap_or_default()) {
        return Ok(key);
    }
    let file = PrivateKeyFile::parse(text)
        .map_err(|_| Error::refused(format!("{} is not a key file.", path.display())))?;
    let comment = if file.is_encrypted() {
        String::new()
    } else {
        file.decrypt(None)?.comment
    };
    Ok(PublicKey {
        key: file.public(),
        comment,
    })
}

/// The bytes an SSHSIG signature covers (PROTOCOL.sshsig).
fn signed_blob(namespace: &str, message_hash: &[u8; 64]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.raw(SIG_MAGIC);
    writer.str(namespace).str("").str(SIG_HASH);
    writer.string(message_hash);
    writer.into_vec()
}

pub fn sign_message(key: &PrivateKey, namespace: &str, message: &[u8]) -> Result<String> {
    let hash = Sha512::digest(message);
    let signature = key.sign(&signed_blob(namespace, &hash))?;
    let mut signature_blob = Writer::new();
    signature_blob.str(HOST_KEY_ALGORITHM).string(&signature);
    let mut blob = Writer::new();
    blob.raw(SIG_MAGIC);
    blob.u32(SIG_VERSION);
    blob.string(&ed25519_public_blob(&key.public().key));
    blob.str(namespace).str("").str(SIG_HASH);
    blob.string(signature_blob.as_slice());
    Ok(armor(blob.as_slice()))
}

fn armor(blob: &[u8]) -> String {
    let encoded = base64(blob);
    let mut out = String::with_capacity(encoded.len() + 64);
    out.push_str(SIG_BEGIN);
    out.push('\n');
    for line in encoded.as_bytes().chunks(70) {
        out.push_str(&String::from_utf8_lossy(line));
        out.push('\n');
    }
    out.push_str(SIG_END);
    out.push('\n');
    out
}

/// Verifies armored SSHSIG text and returns the signer's key.
pub fn verify_signature(armored: &str, namespace: &str, message: &[u8]) -> Result<[u8; 32]> {
    let invalid = || Error::refused("Signature verification failed: invalid format");
    let mut lines = armored.lines().map(str::trim);
    if lines.next() != Some(SIG_BEGIN) {
        return Err(invalid());
    }
    let mut encoded = String::new();
    let mut closed = false;
    for line in lines {
        if line == SIG_END {
            closed = true;
            break;
        }
        encoded.push_str(line);
    }
    if !closed {
        return Err(invalid());
    }
    let blob = from_base64(&encoded).ok_or_else(invalid)?;
    let rest = blob.strip_prefix(SIG_MAGIC).ok_or_else(invalid)?;
    let mut reader = Reader::new(rest);
    if reader.u32().map_err(|_| invalid())? != SIG_VERSION {
        return Err(invalid());
    }
    let public_blob = reader.string().map_err(|_| invalid())?;
    let stated_namespace = reader.str().map_err(|_| invalid())?;
    let reserved = reader.string().map_err(|_| invalid())?;
    let hash_name = reader.str().map_err(|_| invalid())?;
    let signature_blob = reader.string().map_err(|_| invalid())?;
    reader.finish().map_err(|_| invalid())?;
    if stated_namespace != namespace {
        return Err(Error::refused(format!(
            "Signature verification failed: namespace mismatch: signature made with \"{stated_namespace}\", not \"{namespace}\""
        )));
    }
    if !reserved.is_empty() || hash_name != SIG_HASH {
        return Err(Error::refused(format!(
            "Signature verification failed: unsupported hash {hash_name}"
        )));
    }
    let public = parse_ed25519_public(public_blob)
        .map_err(|_| Error::refused("Signature verification failed: not an Ed25519 key"))?;
    let signature = crate::kex::parse_ed25519_signature(signature_blob).map_err(|_| invalid())?;
    let hash = Sha512::digest(message);
    if ed25519::verify(&public, &signed_blob(namespace, &hash), &signature).is_err() {
        return Err(Error::refused(
            "Signature verification failed: bad signature",
        ));
    }
    Ok(public)
}

/// `principal[,principal] ssh-ed25519 KEY [comment]` lines: no options.
pub fn signer_allowed(allowed: &str, identity: &str, key: &[u8; 32]) -> bool {
    for line in allowed.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((principals, public_line)) = line.split_once([' ', '\t']) else {
            continue;
        };
        let names_identity = principals.split(',').any(|name| name == identity);
        if names_identity {
            if let Ok(public) = PublicKey::parse_line(public_line) {
                if public.key == *key {
                    return true;
                }
            }
        }
    }
    false
}

fn sign_command(arguments: &Arguments) -> Result<i32> {
    let namespace = arguments
        .namespace
        .as_deref()
        .ok_or_else(|| Error::refused(USAGE))?;
    let key = load_private(required_file(arguments)?, None, false)?;
    match arguments.positional.as_slice() {
        [] => {
            let mut message = Vec::new();
            io::stdin()
                .lock()
                .read_to_end(&mut message)
                .map_err(Error::from)?;
            primary_output(&sign_message(&key, namespace, &message)?)?;
        }
        files => {
            for name in files {
                let message = fs::read(name)
                    .map_err(|error| Error::refused(format!("{}: {error}", name.display())))?;
                let armored = sign_message(&key, namespace, &message)?;
                let out = with_suffix(name, ".sig");
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o644)
                    .open(&out)
                    .and_then(|mut file| file.write_all(armored.as_bytes()))
                    .map_err(|error| Error::refused(format!("{}: {error}", out.display())))?;
                if !arguments.quiet {
                    diagnostic(&format!("Write signature to {}", out.display()));
                }
            }
        }
    }
    Ok(0)
}

fn verify_command(arguments: &Arguments) -> Result<i32> {
    let namespace = arguments
        .namespace
        .as_deref()
        .ok_or_else(|| Error::refused(USAGE))?;
    let identity = arguments
        .identity
        .as_deref()
        .ok_or_else(|| Error::refused(USAGE))?;
    let signature_path = arguments
        .signature
        .as_deref()
        .ok_or_else(|| Error::refused(USAGE))?;
    let allowed = read_bounded(required_file(arguments)?)?;
    let armored = read_bounded(signature_path)?;
    let mut message = Vec::new();
    io::stdin()
        .lock()
        .read_to_end(&mut message)
        .map_err(Error::from)?;
    let key = verify_signature(&armored, namespace, &message)?;
    if !signer_allowed(&allowed, identity, &key) {
        return Err(Error::refused(format!(
            "Could not verify signature: {identity} is not an allowed signer for this key"
        )));
    }
    if !arguments.quiet {
        primary_output(&format!("Good \"{namespace}\" signature for {identity}\n"))?;
    }
    Ok(0)
}

pub fn run(args: &[OsString]) -> Result<i32> {
    let arguments = parse(args)?;
    let reading_public = arguments.print_public
        && arguments.sign_action.is_none()
        && !arguments.fingerprint
        && arguments.generate_type.is_none();
    if arguments.old_passphrase.is_some() && !reading_public {
        return Err(Error::refused("-P is only for -y"));
    }
    if let Some(action) = arguments.sign_action.as_deref() {
        return match action {
            "sign" => sign_command(&arguments),
            "verify" => verify_command(&arguments),
            other => Err(Error::refused(format!(
                "-Y {other} is not supported by td-ssh-keygen"
            ))),
        };
    }
    if arguments.print_public {
        return print_public(&arguments);
    }
    if arguments.fingerprint {
        return fingerprint(&arguments);
    }
    if arguments.generate_type.is_some() || arguments.new_passphrase.is_some() {
        return generate(&arguments);
    }
    Err(Error::refused(USAGE))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn os_args(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn signatures_verify_and_bind_namespace_message_and_key() {
        let key = PrivateKey::from_seed(&[11; 32], "").unwrap();
        let armored = sign_message(&key, "git", b"hello").unwrap();
        assert_eq!(
            verify_signature(&armored, "git", b"hello").unwrap(),
            key.public().key
        );
        assert!(verify_signature(&armored, "file", b"hello").is_err());
        assert!(verify_signature(&armored, "git", b"other").is_err());
        assert!(verify_signature("nonsense", "git", b"hello").is_err());
        let tampered = armored.replacen('A', "B", 1);
        assert!(verify_signature(&tampered, "git", b"hello").is_err());
    }

    #[test]
    fn allowed_signers_require_the_identity_and_the_key_together() {
        let key = PrivateKey::from_seed(&[12; 32], "").unwrap().public();
        let other = PrivateKey::from_seed(&[13; 32], "").unwrap().public();
        let allowed = format!("# comment\n\nalice,bob {}", key.to_line());
        assert!(signer_allowed(&allowed, "alice", &key.key));
        assert!(signer_allowed(&allowed, "bob", &key.key));
        assert!(!signer_allowed(&allowed, "carol", &key.key));
        assert!(!signer_allowed(&allowed, "alice", &other.key));
        // An option is not a form this reads: the line admits no one.
        let with_option = format!("alice namespaces=\"git\" {}", key.to_line());
        assert!(!signer_allowed(&with_option, "alice", &key.key));
    }

    #[test]
    fn a_small_order_key_is_no_signer_and_verifies_nothing() {
        let mut identity = [0u8; 32];
        identity[0] = 1;
        let blob = ed25519_public_blob(&identity);
        let allowed = format!("alice {HOST_KEY_ALGORITHM} {}\n", base64(&blob));
        assert!(!signer_allowed(&allowed, "alice", &identity));

        // The universal signature, over any message at all.
        let mut universal = [0u8; 64];
        universal[0] = 1;
        let mut signature_blob = Writer::new();
        signature_blob.str(HOST_KEY_ALGORITHM).string(&universal);
        let mut armored_blob = Writer::new();
        armored_blob.raw(SIG_MAGIC);
        armored_blob.u32(SIG_VERSION);
        armored_blob.string(&blob);
        armored_blob.str("git").str("").str(SIG_HASH);
        armored_blob.string(signature_blob.as_slice());
        let armored = armor(armored_blob.as_slice());
        let error = verify_signature(&armored, "git", b"anything")
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("not an Ed25519 key"), "{error}");
    }

    #[test]
    fn arguments_parse_in_the_forms_callers_use() {
        let args = os_args(&["-q", "-t", "ed25519", "-N", "", "-C", "c", "-f", "k"]);
        let parsed = parse(&args).unwrap();
        assert!(parsed.quiet);
        assert_eq!(parsed.generate_type.as_deref(), Some("ed25519"));
        assert_eq!(parsed.new_passphrase.as_deref(), Some(""));
        assert_eq!(parsed.file, Some(PathBuf::from("k")));
        assert!(parse(&os_args(&["-Z"])).is_err());
    }

    #[test]
    fn a_path_keeps_its_bytes_and_only_a_path_may_be_other_than_text() {
        let name = OsString::from_vec(b"k\xff".to_vec());
        let attached = OsString::from_vec(b"-fk\xff".to_vec());
        let path = Some(PathBuf::from(&name));
        for args in [
            vec![OsString::from("-f"), name.clone()],
            vec![attached.clone()],
        ] {
            assert_eq!(parse(&args).unwrap().file, path);
        }
        let parsed = parse(&[OsString::from("-s"), name.clone(), name.clone()]).unwrap();
        assert_eq!(parsed.signature, path);
        assert_eq!(parsed.positional, [PathBuf::from(&name)]);
        for flag in ["-t", "-N", "-P", "-C", "-E", "-Y", "-n", "-I"] {
            let error = parse(&[OsString::from(flag), name.clone()])
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("not UTF-8"), "{flag}: {error}");
        }
        let error = parse(&[OsString::from_vec(b"-\xff".to_vec())])
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains(r"unknown option -- \xff"), "{error}");
    }

    #[test]
    fn a_key_at_a_path_that_is_not_text_is_written_there() {
        let dir = td_fs::TempDir::new("td-ssh-keygen").unwrap();
        let mut name = dir.path().as_os_str().as_bytes().to_vec();
        name.extend_from_slice(b"/k\xff");
        let path = PathBuf::from(OsString::from_vec(name));
        assert_eq!(run(&generate_args(&path)).unwrap(), 0);
        assert!(PrivateKeyFile::parse(&fs::read_to_string(&path).unwrap()).is_ok());
        assert!(fs::symlink_metadata(public_path_of(&path)).is_ok());
    }

    /// Asked for a protected key, or not saying which, leaves nothing behind.
    #[test]
    fn only_an_unprotected_key_is_written_and_only_when_asked_for_by_name() {
        let dir = td_fs::TempDir::new("td-ssh-keygen").unwrap();
        let path = dir.path().join("k");
        let path_arg = path.clone().into_os_string();
        for flags in [
            &["-q", "-t", "ed25519", "-N", "x", "-f"][..],
            &["-q", "-t", "ed25519", "-f"],
        ] {
            let mut args = os_args(flags);
            args.push(path_arg.clone());
            let error = run(&args).err().unwrap().to_string();
            assert!(error.contains("only unprotected keys"), "{error}");
            assert!(fs::symlink_metadata(&path).is_err());
        }
        let mut args = os_args(&["-q", "-t", "ed25519", "-N", "", "-P", "", "-f"]);
        args.push(path_arg);
        let error = run(&args).err().unwrap().to_string();
        assert!(error.contains("only for -y"), "{error}");
        assert!(fs::symlink_metadata(&path).is_err());
        for flags in [
            &["-q", "-Y", "sign", "-P", "x", "-n", "n", "-f", "k"][..],
            &["-l", "-P", "x", "-f", "k"],
            &["-y", "-Y", "sign", "-P", "x", "-n", "n", "-f", "k"],
        ] {
            let error = run(&os_args(flags)).err().unwrap().to_string();
            assert!(error.contains("only for -y"), "{flags:?}: {error}");
        }
    }

    #[test]
    fn generation_refuses_other_types_and_an_existing_file() {
        let dir = td_fs::TempDir::new("td-ssh-keygen").unwrap();
        let path = dir.path().join("k");
        let base = |extra: &[&str]| -> Vec<OsString> {
            let mut args = os_args(&["-q", "-N", "", "-C", "c", "-f"]);
            args.push(path.clone().into_os_string());
            args.extend(os_args(extra));
            args
        };
        assert!(run(&base(&["-t", "rsa"])).is_err());
        assert_eq!(run(&base(&["-t", "ed25519"])).unwrap(), 0);
        assert!(run(&base(&["-t", "ed25519"])).is_err());
        let text = fs::read_to_string(&path).unwrap();
        assert!(PrivateKeyFile::parse(&text).is_ok());
        let public = fs::read_to_string(public_path_of(&path)).unwrap();
        assert_eq!(PublicKey::parse_line(&public).unwrap().comment, "c");
        let hostile = PublicKey::parse_line(&format!("{} a\x1b[2Jb", public.trim())).unwrap();
        let line = fingerprint_line(&hostile);
        assert!(!line.contains('\x1b'), "{line:?}");
        assert!(line.contains("a?[2Jb"), "{line:?}");
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    }

    fn generate_args(path: &Path) -> Vec<OsString> {
        let mut args = os_args(&["-q", "-t", "ed25519", "-N", "", "-f"]);
        args.push(path.as_os_str().to_os_string());
        args
    }

    #[test]
    fn an_existing_public_half_leaves_no_new_private_key() {
        let dir = td_fs::TempDir::new("td-ssh-keygen").unwrap();
        let path = dir.path().join("k");
        let public = public_path_of(&path);
        fs::write(&public, "keep me\n").unwrap();
        let error = run(&generate_args(&path)).err().unwrap().to_string();
        assert!(error.contains("already exists"), "{error}");
        assert!(fs::symlink_metadata(&path).is_err());
        assert_eq!(fs::read_to_string(&public).unwrap(), "keep me\n");
    }

    #[test]
    fn an_existing_private_key_is_kept_with_no_public_half_made() {
        let dir = td_fs::TempDir::new("td-ssh-keygen").unwrap();
        let path = dir.path().join("k");
        fs::write(&path, "keep me\n").unwrap();
        assert!(run(&generate_args(&path)).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "keep me\n");
        assert!(fs::symlink_metadata(public_path_of(&path)).is_err());
    }

    #[test]
    fn a_public_half_that_cannot_be_written_takes_the_private_key_with_it() {
        let dir = td_fs::TempDir::new("td-ssh-keygen").unwrap();
        let path = dir.path().join("k");
        let unwritable = dir.path().join("absent/k.pub");
        assert!(write_pair(&path, "private", &unwritable, "public").is_err());
        assert!(fs::symlink_metadata(&path).is_err());
        let public = dir.path().join("k.pub");
        write_pair(&path, "private", &public, "public").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "private");
        assert_eq!(fs::read_to_string(&public).unwrap(), "public");
    }
}
