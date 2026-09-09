// Included only beneath the ignored root/QEMU desktop tests.
use super::*;

const INPUT_READY: &str = "/run/td-secret-system-input-ready";
const VALUE: &[u8] = b"system fixture credential";
const CONFIG: &str = "/var/lib/td/applications/65537/.td/app/mail/config/td-mail/config.toml";
const STRIDE: usize = 1280 * 4;
const ATTENTION_BACKGROUND: &[u8] = &[0x28, 0x20, 0x18];

fn notice_pixels() -> Vec<u8> {
    use std::os::unix::fs::FileExt;
    let mut pixels = vec![0; STRIDE * 14];
    // The full-system QEMU output is fixed at 1280x800, with 32-bit pixels.
    File::open("/dev/fb0").unwrap().read_exact_at(&mut pixels, (STRIDE * 330) as u64).unwrap();
    pixels
}

fn notice(text: &str) {
    // Independent bitmap expectations for the visible 5x7 status face.
    let glyphs: Vec<[u8; 7]> = text.bytes().map(|byte| match byte {
        b' ' => [0; 7],
        b':' => [0, 4, 4, 0, 4, 4, 0],
        b'A' => [14, 17, 17, 31, 17, 17, 17],
        b'C' => [14, 17, 16, 16, 16, 17, 14],
        b'D' => [30, 17, 17, 17, 17, 17, 30],
        b'E' => [31, 16, 16, 30, 16, 16, 31],
        b'I' => [14, 4, 4, 4, 4, 4, 14],
        b'K' => [17, 18, 20, 24, 20, 18, 17],
        b'L' => [16, 16, 16, 16, 16, 16, 31],
        b'N' => [17, 25, 21, 19, 17, 17, 17],
        b'O' => [14, 17, 17, 17, 17, 17, 14],
        b'R' => [30, 17, 17, 30, 20, 18, 17],
        b'S' => [15, 16, 16, 14, 1, 1, 30],
        b'T' => [31, 4, 4, 4, 4, 4, 4],
        b'U' => [17, 17, 17, 17, 17, 17, 14],
        _ => panic!("unsupported notice glyph"),
    }).collect();
    wait(&format!("visible attention notice {text}"), || {
        let pixels = notice_pixels();
        glyphs.iter().enumerate().all(|(column, glyph)| glyph.iter().enumerate().all(|(row, bits)| {
            (0..6).all(|x| {
                let at = row * 2 * STRIDE + (24 + column * 12 + x * 2) * 4;
                let ink = x < 5 && bits & (1 << (4 - x)) != 0;
                &pixels[at..at + 3] == if ink { &[255, 255, 255] } else { ATTENTION_BACKGROUND }
            })
        }))
    });
    eprintln!("system visible notice: {text}");
}

fn select(keyboard: &mut Keyboard, key: u8) {
    assert!(fs::read_dir("/sys/class/input").unwrap().any(|entry| {
        let path = entry.unwrap().path();
        path.file_name().unwrap().to_str().unwrap().starts_with("event")
            && fs::read_to_string(path.join("device/name")).ok().as_deref() == Some("td desktop keyboard\n")
    }), "fixture keyboard disappeared");
    keyboard.key(0x39); // Fresh report drains the post-close quarantine.
    keyboard.report(5, 0);
    keyboard.report(5, 0x29);
    keyboard.report(0, 0);
    notice("U: UNLOCK");
    keyboard.key(key);
}

fn close(keyboard: &mut Keyboard) {
    use std::os::unix::fs::FileExt;
    keyboard.close();
    let framebuffer = File::open("/dev/fb0").unwrap();
    wait("attention display closed", || {
        let mut pixel = [0; 3];
        framebuffer.read_exact_at(&mut pixel, 0).unwrap();
        pixel != ATTENTION_BACKGROUND
    });
}

fn system_wait(label: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(180);
    while !done() {
        assert!(Instant::now() < deadline, "system fixture timed out: {label}");
        thread::sleep(Duration::from_millis(100));
    }
}

fn service(args: &[&str]) -> String {
    let output = Command::new("/bin/td-svc").args(args).output().unwrap();
    assert!(output.status.success(), "td-svc {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

fn ready(names: &[&str]) {
    system_wait("stock services ready", || {
        let status = service(&["status"]);
        names.iter().all(|name| status.lines().any(|line| line.starts_with(&format!("{name} ready "))))
    });
}

fn receipt_count(locked: bool) -> usize {
    let marker = if locked { "TD-SECRET-LOCKED app=mail name=main" } else { "TD-SECRET-READY app=mail name=main" };
    fs::read_to_string("/run/td-portal.log").unwrap_or_default().lines().filter(|line| *line == marker).count()
}

fn restart_mail(locked: bool) {
    let before = receipt_count(locked);
    service(&["restart", "mail"]);
    system_wait("fresh jailed mail credential response", || receipt_count(locked) > before);
    ready(&["mail"]);
    let output = Command::new("/bin/td-login")
        .args(["exec-service-as", "tda65537", "--", "/bin/td-jail", "--probe-process-token", "mail", "td-mail"])
        .output().unwrap();
    assert!(output.status.success(), "jailed mail process: {}", String::from_utf8_lossy(&output.stderr));
}

fn released(expected: &[u8]) {
    // Polling Store::open before publication can steal the worker's lock.
    wait("system key publication", || Path::new("/run/td-secret/1000/key").exists());
    wait("released store admission", || {
        let Ok(store) = crate::owned_store(1000) else { return false; };
        assert_eq!(store.application_secret("mail", "main").unwrap().unwrap(), expected);
        true
    });
}

fn checked_token(token: &VirtualCredential, expected: &'static [u8], requests: Arc<AtomicUsize>, recovery: bool) -> Token {
    token.checked(move |request, index| {
        use crate::fido_cbor::{self, Value};
        eprintln!("system CTAP recovery={recovery} index={index} command={}", request[0]);
        assert_eq!(request[0], expected[index]);
        if request[0] != 4 {
            let value = fido_cbor::decode(&request[1..]).unwrap();
            if request[0] == 1 {
                let user = value.required(&Value::Unsigned(3)).unwrap()
                    .required(&Value::Text("id")).unwrap().bytes().unwrap();
                let path = format!("{COLD_STATE}/user");
                if recovery {
                    assert_eq!(fs::read(path).unwrap(), user);
                    let Value::Array(excluded) = value.required(&Value::Unsigned(5)).unwrap() else { panic!("missing primary exclusion"); };
                    assert_eq!(excluded.len(), 1);
                    assert_eq!(excluded[0].required(&Value::Text("id")).unwrap().bytes().unwrap(), [44; 32]);
                } else { fs::write(path, user).unwrap(); }
            }
            let field = if request[0] == 1 { 1 } else { 2 };
            let hash: [u8; 32] = value.required(&Value::Unsigned(field)).unwrap().bytes().unwrap().try_into().unwrap();
            assert_ne!(hash, [0; 32]);
            let path = format!("{COLD_STATE}/challenges");
            let prior = fs::read(&path).unwrap_or_default();
            assert!(prior.len().is_multiple_of(32));
            assert!(!prior.as_chunks::<32>().0.contains(&hash), "reused token challenge");
            OpenOptions::new().append(true).create(true).mode(0o600).open(path).unwrap().write_all(&hash).unwrap();
        }
        requests.store(index + 1, Ordering::SeqCst);
    })
}

struct SystemDiagnostics;
impl Drop for SystemDiagnostics {
    fn drop(&mut self) {
        if !thread::panicking() { return; }
        eprintln!("system status: {}", service(&["status"]));
        let pixels = notice_pixels();
        for row in 0..7 {
            let line: String = (0..250).map(|column| {
                let at = row * 2 * STRIDE + (24 + column * 2) * 4;
                if pixels[at..at + 3] == [255, 255, 255] { '#' } else { ' ' }
            }).collect();
            eprintln!("attention pixels: {line}");
        }
        for path in ["/run/td-portal.log", "/run/desktop-set.log", "/var/log/svc/td-profiler.log"] {
            if let Ok(file) = File::open(path) {
                let mut bytes = Vec::new();
                if file.take(65_536).read_to_end(&mut bytes).is_ok() {
                    eprintln!("{path}: {}", String::from_utf8_lossy(&bytes));
                }
            }
        }
    }
}

#[test]
#[ignore = "requires the test-only full system image and retained QEMU TPM/disk"]
fn qemu_installed_system_secret_lifecycle() {
    guard("fido-system");
    let _diagnostics = SystemDiagnostics;
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap();
    let recover = cmdline.split_ascii_whitespace().any(|token| token == "td.secret-system=recover");
    assert!(recover || cmdline.split_ascii_whitespace().any(|token| token == "td.secret-system=create"));
    let mounts = fs::read_to_string("/proc/mounts").unwrap();
    for (mount, kind, flags) in [("/", "erofs", &["ro"][..]), ("/var", "btrfs", &["rw", "nosuid", "nodev"][..])] {
        assert!(mounts.lines().any(|line| {
            let fields: Vec<_> = line.split_ascii_whitespace().collect();
            fields.get(1) == Some(&mount) && fields.get(2) == Some(&kind)
                && flags.iter().all(|flag| fields[3].split(',').any(|part| part == *flag))
        }), "missing mounted {mount} {kind} {flags:?}");
    }
    let initial = if recover {
        let bytes = sealed_bytes();
        assert_eq!(crate::crypto::digest(&bytes).as_slice(), fs::read(format!("{COLD_STATE}/bundle-hash")).unwrap());
        assert_ne!(fs::read("/proc/sys/kernel/random/boot_id").unwrap(), fs::read(format!("{COLD_STATE}/boot-id")).unwrap());
        for (path, saved) in [(CONFIG, "config"), ("/var/lib/td/principals.tsv", "principals"), ("/var/lib/td/machine-id", "machine-id")] {
            assert_eq!(fs::read(path).unwrap(), fs::read(format!("{COLD_STATE}/{saved}")).unwrap(), "firstboot changed {path}");
        }
        assert!(!store::user_path(1000).join("master").exists());
        assert!(!store::user_path(1000).join("mail.main").exists());
        bytes
    } else {
        assert!(!Path::new(COLD_STATE).exists());
        assert!(store::user_path(1000).join("master").exists());
        assert!(!store::user_path(1000).join("sealed").exists());
        assert_eq!(crate::owned_store(1000).unwrap().get("mail", "main").unwrap().unwrap(), b"replace-me\n");
        Vec::new()
    };
    assert!(fs::read_to_string(CONFIG).unwrap().contains("secret = \"portal\""));
    assert!(!Path::new(CONFIG).with_file_name("password").exists());
    no_release();
    assert!(Device::discover().unwrap().is_empty());
    let mut keyboard = Keyboard::new();
    fs::write(INPUT_READY, b"ready").unwrap();
    ready(&["td-firstboot", "rootcheck", "seat", "busd", "portal", "wayland", "mail", "news"]);
    assert_eq!(fs::read_to_string("/sys/class/graphics/fb0/virtual_size").unwrap().trim(), "1280,800");
    assert_eq!(fs::read_to_string("/sys/class/graphics/fb0/bits_per_pixel").unwrap().trim(), "32");
    assert_eq!(fs::read_to_string("/sys/class/graphics/fb0/stride").unwrap().trim(), STRIDE.to_string());
    system_wait("initial locked mail refusal", || receipt_count(true) > 0);
    assert_eq!(receipt_count(false), 0);
    no_release();
    // Synthetic PCR extension exists only in this explicit, non-shipping fixture.
    crate::tpm::tests::qemu_extend(&[9; 32]);
    let token = persistent_token(!recover, recover);
    let requests = Arc::new(AtomicUsize::new(0));
    let hid = checked_token(&token, if recover { &[4, 2] } else { &[4, 1, 2, 4, 2, 4, 2] }, Arc::clone(&requests), false);
    discover_one();
    restart_mail(true);
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    if !recover {
        select(&mut keyboard, 0x08); // E.
        wait("primary proof request", || requests.load(Ordering::SeqCst) == 3);
        let second = persistent_token(true, true);
        assert_ne!(token.signer.lock().unwrap().cose, second.signer.lock().unwrap().cose);
        let second_requests = Arc::new(AtomicUsize::new(0));
        let second_hid = checked_token(&second, &[4, 1, 2], Arc::clone(&second_requests), true);
        wait("system enrollment", || store::user_path(1000).join("sealed").exists());
        notice("STORE ENROLLED");
        no_release();
        assert_eq!(second_hid.finish(), (3, 0));
        wait("second token removal", || Device::discover().unwrap().len() == 1);
        close(&mut keyboard);
    } else {
        assert_eq!(fs::read(format!("{COLD_STATE}/challenges")).unwrap().len(), 6 * 32);
    }
    select(&mut keyboard, if recover { 0x15 } else { 0x18 }); // R or U.
    released(if recover { VALUE } else { b"replace-me\n" });
    notice("SECRETS UNLOCKED");
    restart_mail(false);
    if !recover {
        close(&mut keyboard);
        let before = sealed_bytes();
        let mut command = Command::new("/bin/td-login");
        command.args(["exec-as", "tester", "--", "/bin/td-secret", "set", "mail/main"]).stdin(Stdio::piped());
        let mut client = Process::start(command, "/run/desktop-set.log");
        client.0.stdin.take().unwrap().write_all(VALUE).unwrap();
        wait("system write queued", || {
            assert!(client.exited().is_none());
            fs::read_to_string("/run/desktop-set.log").unwrap().contains("then W")
        });
        assert_eq!(sealed_bytes(), before);
        assert_eq!(requests.load(Ordering::SeqCst), 5);
        select(&mut keyboard, 0x1a); // W.
        let mut status = None;
        wait("system write finished", || { status = client.exited(); status.is_some() });
        assert!(status.unwrap().success());
        notice("CREDENTIAL STORED");
        released(VALUE);
        restart_mail(false);
    }
    let bundle = sealed_bytes();
    close(&mut keyboard);
    service(&["restart", "wayland"]);
    ready(&["wayland"]);
    no_release();
    restart_mail(true);
    assert_eq!(sealed_bytes(), bundle);
    assert_eq!(hid.finish(), if recover { (2, 0) } else { (7, 0) });
    assert!(Device::discover().unwrap().is_empty());
    if recover {
        assert_eq!(bundle, initial, "recovery changed the persistent bundle");
    } else {
        fs::write(format!("{COLD_STATE}/bundle-hash"), crate::crypto::digest(&bundle)).unwrap();
        for (path, saved) in [(CONFIG, "config"), ("/var/lib/td/principals.tsv", "principals"), ("/var/lib/td/machine-id", "machine-id"), ("/proc/sys/kernel/random/boot_id", "boot-id")] {
            fs::copy(path, format!("{COLD_STATE}/{saved}")).unwrap();
        }
    }
    // The wrapper validates libtest's final summary, then asks stock td-svc
    // to stop every service and run the unchanged persistent shutdown path.
}
