//! A serial byte from the guest proves it ran on KVM before the crash.
use std::fs::{self, File};
use std::io;
use std::os::unix::{fs::FileExt, net::UnixDatagram};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use td_fs::TempDir;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Fixture {
    root: TempDir,
    bin: PathBuf,
}

impl Fixture {
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_td-vm"));
        cmd.env("TD_VM_HOME", self.root.join("state"))
            .env("PATH", &self.bin)
            .stdin(Stdio::null());
        cmd
    }

    fn invoke(&self, args: &[&str], success: bool) -> Result<Output> {
        let out = self.command().args(args).output()?;
        assert_eq!(
            out.status.success(),
            success,
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(out)
    }

    fn run(&self, name: &str) -> Command {
        let mut cmd = self.command();
        cmd.args(["run-native", name])
            .arg(self.root.join("kernel"))
            .arg(self.root.join("initrd"))
            .arg("");
        cmd
    }

    fn launch(&self, name: &str) -> Result<Guest> {
        let serial = self.root.join(name);
        let supervisor = self.run(name).stdout(File::create(&serial)?).spawn()?;
        let mut guest = Guest {
            supervisor,
            control: self.root.join("state/instances").join(name).join("ctl"),
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        while fs::read(&serial)? != b"K\n" {
            assert!(
                guest.supervisor.try_wait()?.is_none(),
                "{name} ended before guest serial output"
            );
            assert!(
                Instant::now() < deadline,
                "{name} did not execute the guest"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(guest)
    }
}

struct Guest {
    supervisor: Child,
    control: PathBuf,
}
impl Drop for Guest {
    fn drop(&mut self) {
        // Use the private endpoint even when a panic follows supervisor loss.
        if let Ok(socket) = UnixDatagram::unbound() {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let _ = socket.send_to(b"stop", &self.control);
                if self.supervisor.try_wait().ok().flatten().is_some() || Instant::now() >= deadline
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let _ = self.supervisor.kill();
        let _ = self.supervisor.wait();
    }
}

fn guest_image(root: &Path) -> io::Result<()> {
    // A Linux boot-protocol header followed by x86 protected-mode code.
    // Emit K and LF to COM1, then halt with interrupts disabled.
    let mut image = [0u8; 4096 + 13];
    image[0x1f1] = 7;
    image[0x1fe..0x200].copy_from_slice(&[0x55, 0xaa]);
    image[0x201] = 0x6a;
    image[0x202..0x206].copy_from_slice(b"HdrS");
    image[0x206..0x208].copy_from_slice(&0x20cu16.to_le_bytes());
    image[0x211] = 1;
    image[0x214..0x218].copy_from_slice(&0x100000u32.to_le_bytes());
    image[0x22c..0x230].copy_from_slice(&0x1fffffu32.to_le_bytes());
    image[0x236] = 1;
    image[4096..].copy_from_slice(&[
        0x66, 0xba, 0xf8, 0x03, 0xb0, b'K', 0xee, 0xb0, 10, 0xee, 0xfa, 0xf4, 0xf4,
    ]);
    fs::write(root.join("kernel"), image)?;
    fs::write(root.join("initrd"), [0])
}

#[test]
#[ignore = "td-builder check integration supplies td-vmm and writable KVM"]
fn vm_integration_supervisor_loss_and_independent_stop() -> Result<()> {
    let vmm = PathBuf::from(std::env::var_os("TD_TEST_VMM").ok_or("missing TD_TEST_VMM")?);
    let root = TempDir::new_in(Path::new("/tmp"), "vm")?;
    let bin = root.join("bin");
    fs::create_dir(&bin)?;
    std::os::unix::fs::symlink(vmm.canonicalize()?, bin.join("td-vmm"))?;
    let fixture = Fixture { root, bin };
    guest_image(fixture.root.path())?;
    let source = fixture.root.join("base");
    fs::write(&source, [0x12; 512])?;
    for name in ["one", "two"] {
        fixture.invoke(
            &[
                "create-native",
                name,
                source.to_str().ok_or("fixture path")?,
            ],
            true,
        )?;
    }
    let instances = fixture.root.join("state/instances");
    let disk_one = instances.join("one/disk.raw");
    let disk_two = instances.join("two/disk.raw");
    File::options()
        .write(true)
        .open(&disk_one)?
        .write_all_at(b"one", 0)?;
    assert_eq!(fs::read(&source)?, [0x12; 512]);
    assert_eq!(fs::read(&disk_two)?, [0x12; 512]);
    let mut one = fixture.launch("one")?;
    let mut two = fixture.launch("two")?;
    // Drop the real supervisor's descriptors, leaving only the VMM holding the lock.
    two.supervisor.kill()?;
    assert!(!two.supervisor.wait()?.success());
    for name in ["one", "two"] {
        let status = fixture.invoke(&["status", name], true)?;
        assert!(String::from_utf8(status.stdout)?.contains("live (KVM)"));
        let lock = File::open(fixture.root.join("state/locks").join(format!("run-{name}")))?;
        assert!(matches!(
            lock.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        assert!(
            matches!(td_vm::disk::lock_idle(&instances.join(name).join("disk.raw")), Err(e) if e.kind() == io::ErrorKind::WouldBlock)
        );
        let duplicate = fixture.run(name).output()?;
        assert!(!duplicate.status.success());
        assert!(String::from_utf8(duplicate.stderr)?.contains("another operation holds run-"));
        fixture.invoke(&["delete", name, "--yes"], false)?;
    }
    fixture.invoke(&["stop", "one", "--force"], true)?;
    assert!(one.supervisor.wait()?.success());
    fixture.invoke(&["delete", "one", "--yes"], true)?;
    assert!(!disk_one.exists());
    assert!(
        String::from_utf8(fixture.invoke(&["status", "two"], true)?.stdout)?.contains("live (KVM)")
    );
    fixture.invoke(&["delete", "two", "--yes"], false)?;
    fixture.invoke(&["stop", "two", "--force"], true)?;
    assert_eq!(fs::read(&disk_two)?, [0x12; 512]);
    let lock = td_vm::disk::lock_idle(&disk_two)?;
    fixture.invoke(&["delete", "two", "--yes"], false)?;
    drop(lock);
    fixture.invoke(&["delete", "two", "--yes"], true)?;
    assert!(!disk_two.exists());
    Ok(())
}
