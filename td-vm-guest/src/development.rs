//! Private writable state for development builds in one VM.
use super::{io, Result};
use std::fs::{self, DirBuilder, File};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

#[cfg(feature = "target-recipe")]
const HOME: &str = "/home/tester";
#[cfg(any(test, feature = "target-recipe"))]
const HOME_BACKING: &str = "/var/home/tester";
#[cfg(any(test, feature = "target-recipe"))]
const TD_STATE_BACKING: &str = "/var/home/tester/.td";
#[cfg(feature = "target-recipe")]
const WORK: &str = "/home/tester/src/td-vm/work";
#[cfg(any(test, feature = "target-recipe"))]
const WORK_BACKING: &str = "/var/home/tester/src/td-vm/work";
#[cfg(any(test, feature = "target-recipe"))]
const WORK_CACHE_BACKING: &str = "/var/home/tester/src/td-vm/work/.td-build-cache";

#[cfg(any(test, feature = "target-recipe"))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mount {
    point: String,
    fstype: String,
    options: Vec<String>,
}

#[cfg(any(test, feature = "target-recipe"))]
impl Mount {
    fn has(&self, option: &str) -> bool {
        self.options.iter().any(|value| value == option)
    }
}

#[cfg(any(test, feature = "target-recipe"))]
fn unescape(field: &str) -> Option<String> {
    let bytes = field.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes.get(at) == Some(&b'\\') {
            let digits = bytes.get(at.saturating_add(1)..at.saturating_add(4))?;
            if digits.len() != 3 || !digits.iter().all(|byte| matches!(byte, b'0'..=b'7')) {
                return None;
            }
            let value = u16::from(*digits.first()? - b'0') * 64
                + u16::from(*digits.get(1)? - b'0') * 8
                + u16::from(*digits.get(2)? - b'0');
            let value = u8::try_from(value).ok()?;
            if value == 0 {
                return None;
            }
            output.push(value);
            at = at.saturating_add(4);
        } else {
            output.push(*bytes.get(at)?);
            at = at.saturating_add(1);
        }
    }
    String::from_utf8(output).ok()
}

#[cfg(any(test, feature = "target-recipe"))]
fn covers(point: &str, path: &str) -> bool {
    if point == "/" {
        return path.starts_with('/');
    }
    let point = point.trim_end_matches('/');
    path.strip_prefix(point)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

#[cfg(any(test, feature = "target-recipe"))]
fn parse_mounts(contents: &str) -> Option<Vec<Mount>> {
    let mut mounts = Vec::new();
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        let (_source, point, fstype, options) = (
            fields.next()?,
            fields.next()?,
            fields.next()?,
            fields.next()?,
        );
        mounts.push(Mount {
            point: unescape(point)?,
            fstype: unescape(fstype)?,
            options: options.split(',').map(String::from).collect(),
        });
    }
    Some(mounts)
}

#[cfg(any(test, feature = "target-recipe"))]
fn covering(mounts: &[Mount], path: &str) -> Option<Mount> {
    let mut best: Option<Mount> = None;
    for mount in mounts {
        let point = &mount.point;
        if !covers(point, path) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|old| point.len() >= old.point.len())
        {
            best = Some(mount.clone());
        }
    }
    best
}

#[cfg(any(test, feature = "target-recipe"))]
fn require_layout(mounts: &str, home: &Path, work: &Path, store: &Path) -> Result<()> {
    if home != Path::new(HOME_BACKING)
        || work != Path::new(WORK_BACKING)
        || store != Path::new("/td/store")
    {
        return Err("development paths do not resolve into the standard persistent home".into());
    }
    let mounts = parse_mounts(mounts).ok_or("development mount table is malformed")?;
    let persistent = covering(&mounts, HOME_BACKING)
        .ok_or("no filesystem covers the private development home")?;
    if persistent.point != "/var"
        || persistent.fstype != "btrfs"
        || !persistent.has("rw")
        || !persistent.has("nodev")
        || !persistent.has("nosuid")
        || persistent.has("noexec")
    {
        return Err(
            "private development state requires executable rw,nodev,nosuid btrfs mounted at /var"
                .into(),
        );
    }
    for root in [TD_STATE_BACKING, WORK_CACHE_BACKING] {
        if covering(&mounts, root).as_ref() != Some(&persistent)
            || mounts.iter().any(|mount| covers(root, &mount.point))
        {
            return Err("private development state crosses an unexpected mount".into());
        }
    }
    let deployed =
        covering(&mounts, "/td/store").ok_or("no filesystem covers the deployed td store")?;
    if deployed.point != "/" || deployed.fstype != "erofs" || !deployed.has("ro") {
        return Err("deployed /td/store must remain on the read-only erofs root".into());
    }
    if mounts.iter().any(|mount| covers("/td/store", &mount.point)) {
        return Err("deployed /td/store contains a nested mount".into());
    }
    Ok(())
}

fn private_directory(path: &Path, uid: u32) -> Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(format!(
                "create private development directory {}: {error}",
                path.display()
            ))
        }
    }
    let metadata = io(
        fs::symlink_metadata(path),
        "inspect private development directory",
    )?;
    if !metadata.is_dir() || metadata.uid() != uid {
        return Err(format!(
            "{} is not a directory owned by the development user",
            path.display()
        ));
    }
    if metadata.mode() & 0o7777 != 0o700 {
        io(
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)),
            "make development directory private",
        )?;
    }
    Ok(())
}

fn prepare_directories(home: &Path, work: &Path, uid: u32) -> Result<()> {
    let roots = [
        home.join(".td"),
        home.join(".td/build-daemon"),
        home.join(".td/build-daemon/ladder-shared-v1"),
        home.join(".td/build-daemon/ladder-shared-v1/seed-store"),
        home.join(".td/build-daemon/ladder-shared-v1/seed-db"),
        home.join(".td/build-daemon/ladder-shared-v1/build-cache"),
        home.join(".td/build-daemon/ladder-shared-v1/build-cache/store"),
        home.join(".td/build-daemon/ladder-shared-v1/scratch"),
        home.join(".td/sources"),
        home.join(".td/ostree"),
        work.join(".td-build-cache"),
    ];
    for path in &roots {
        private_directory(path, uid)?;
    }
    for path in roots.iter().rev() {
        let action = format!("sync private development directory {}", path.display());
        io(
            File::open(path).and_then(|directory| directory.sync_all()),
            &action,
        )?;
    }
    for path in [work, home] {
        let action = format!("sync private development parent {}", path.display());
        io(
            File::open(path).and_then(|directory| directory.sync_all()),
            &action,
        )?;
    }
    Ok(())
}

pub fn prepare(home: &Path, work: &Path, uid: u32) -> Result<()> {
    #[cfg(feature = "target-recipe")]
    {
        if home != Path::new(HOME) || work != Path::new(WORK) {
            return Err("development preparation requires the standard task worktree".into());
        }
        let canonical_home = io(fs::canonicalize(home), "resolve development home")?;
        let canonical_work = io(fs::canonicalize(work), "resolve task worktree")?;
        let canonical_store = io(fs::canonicalize("/td/store"), "resolve deployed td store")?;
        let mounts = io(
            fs::read_to_string("/proc/mounts"),
            "read development mounts",
        )?;
        require_layout(&mounts, &canonical_home, &canonical_work, &canonical_store)?;
    }
    prepare_directories(home, work, uid)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::DirBuilderExt;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};

    const IMAGE: &str = "\
/dev/loop0 / erofs ro,relatime 0 0\n\
tmpfs /run tmpfs rw,nosuid,nodev 0 0\n\
tmpfs /tmp tmpfs rw,nosuid,nodev 0 0\n\
/dev/vda /var btrfs rw,nodev,nosuid,relatime,subvol=/@var 0 0\n";

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("td-vm-development-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            DirBuilder::new().mode(0o700).create(&root).unwrap();
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(root.join("home/src/td-vm/work"))
                .unwrap();
            Self(root)
        }
        fn home(&self) -> PathBuf {
            self.0.join("home")
        }
        fn work(&self) -> PathBuf {
            self.0.join("home/src/td-vm/work")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn image_layout_separates_private_physical_state_from_the_logical_store() {
        assert!(require_layout(
            IMAGE,
            Path::new(HOME_BACKING),
            Path::new(WORK_BACKING),
            Path::new("/td/store")
        )
        .is_ok());
        for bad in [
            IMAGE.replace("btrfs rw", "btrfs ro"),
            IMAGE.replace("rw,nodev,nosuid", "rw,nosuid"),
            IMAGE.replace("rw,nodev,nosuid", "rw,nodev,nosuid,noexec"),
            IMAGE.replace("/ erofs ro", "/ erofs rw"),
            IMAGE.replace("/ erofs ro", "/ ext4 ro"),
            format!("{IMAGE}tmpfs /var tmpfs rw,nodev,nosuid 0 0\n"),
            format!(
                "{IMAGE}tmpfs /var/home/tester/src tmpfs rw,nodev,nosuid 0 0\n"
            ),
            format!(
                "{IMAGE}tmpfs /var/home/tester/.td tmpfs rw,nodev,nosuid 0 0\n"
            ),
            format!(
                "{IMAGE}tmpfs /var/home/tester/src/td-vm/work/.td-build-cache tmpfs rw,nodev,nosuid,noexec 0 0\n"
            ),
            format!("{IMAGE}tmpfs /td/store tmpfs rw,nodev,nosuid 0 0\n"),
            format!("{IMAGE}tmpfs /td/store/shadow tmpfs rw,nodev,nosuid 0 0\n"),
        ] {
            assert!(
                require_layout(
                    &bad,
                    Path::new(HOME_BACKING),
                    Path::new(WORK_BACKING),
                    Path::new("/td/store")
                )
                .is_err(),
                "accepted bad mount layout:\n{bad}"
            );
        }
        assert!(require_layout(
            IMAGE,
            Path::new(HOME_BACKING),
            Path::new(WORK_BACKING),
            Path::new("/var/td-store-link")
        )
        .is_err());
        assert!(require_layout(
            &format!("{IMAGE}/dev/vda /var/home/tester/Downloads none rw,nodev,nosuid,bind 0 0\n"),
            Path::new(HOME_BACKING),
            Path::new(WORK_BACKING),
            Path::new("/td/store")
        )
        .is_ok());
    }

    #[test]
    fn two_guests_prepare_independent_writable_state_concurrently() {
        let first = Fixture::new("first");
        let second = Fixture::new("second");
        let barrier = Arc::new(Barrier::new(2));
        std::thread::scope(|scope| {
            for (fixture, marker) in [
                (&first, b"first".as_slice()),
                (&second, b"second".as_slice()),
            ] {
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    prepare_directories(&fixture.home(), &fixture.work(), current_uid()).unwrap();
                    fs::write(
                        fixture
                            .home()
                            .join(".td/build-daemon/ladder-shared-v1/build-cache/store/probe"),
                        marker,
                    )
                    .unwrap();
                    fs::write(fixture.work().join(".td-build-cache/check"), b"green").unwrap();
                });
            }
        });
        for fixture in [&first, &second] {
            let store = fixture
                .home()
                .join(".td/build-daemon/ladder-shared-v1/build-cache/store");
            assert!(store.is_dir());
            assert!(!fs::read(store.join("probe")).unwrap().is_empty());
            assert_eq!(
                fs::read(fixture.work().join(".td-build-cache/check")).unwrap(),
                b"green"
            );
            prepare_directories(&fixture.home(), &fixture.work(), current_uid()).unwrap();
            assert!(store.join("probe").is_file(), "retry erased build state");
        }
        assert_ne!(first.home(), second.home());
    }

    #[test]
    fn existing_owned_state_is_made_private_but_links_are_refused() {
        let fixture = Fixture::new("bad");
        let private = fixture.home().join(".td");
        DirBuilder::new().mode(0o700).create(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
        prepare_directories(&fixture.home(), &fixture.work(), current_uid()).unwrap();
        assert_eq!(
            fs::symlink_metadata(&private).unwrap().mode() & 0o7777,
            0o700
        );
        fs::remove_dir_all(&private).unwrap();
        std::os::unix::fs::symlink("src", &private).unwrap();
        assert!(prepare_directories(&fixture.home(), &fixture.work(), current_uid()).is_err());

        let other = Fixture::new("other-owner");
        assert!(prepare_directories(&other.home(), &other.work(), current_uid() + 1).is_err());
    }

    fn current_uid() -> u32 {
        fs::metadata("/proc/self").unwrap().uid()
    }
}
