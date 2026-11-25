//! The database transport every operation expects: PostgreSQL listening
//! only on its group-shared private Unix socket, reached by SCRAM.
use super::*;
use crate::{
    isolation::files::{self, Setting},
    physical::{PG_DROPIN, SOCKET_DIR},
};
use fence::PG_FENCE;
use std::{io::Read, os::unix::fs::FileTypeExt};

const PG_DROPIN_TEXT: &str = "[Service]\nSupplementaryGroups=td-mta-db\n";
pub(super) const CONF_LIMIT: u64 = 64 * 1024;
pub(super) const FILE_LIMIT: u64 = 16 * 1024;
// What the server's configuration must give it, as `postgres -C` reports
// it with no fence file present: it prints the mode 0660 in decimal.
const SOCKET_SETTINGS: &[(&str, &str)] = &[
    ("listen_addresses", ""),
    ("ssl", "off"),
    ("port", "35432"),
    ("unix_socket_directories", SOCKET_DIR),
    ("unix_socket_group", isolation::DB_GROUP),
    ("unix_socket_permissions", "432"),
];

pub(super) fn role_name(s: &str) -> bool {
    s.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

pub(super) fn check_socket(directory: &Path, uid: u32, gid: u32, mode: u32) -> R<()> {
    let dir = fs::symlink_metadata(directory)?;
    require(
        dir.is_dir() && dir.uid() == uid && dir.gid() == gid && dir.mode() & 0o7777 == 0o750,
        "PostgreSQL socket directory ownership or mode",
    )?;
    let socket = fs::symlink_metadata(child(directory, ".s.PGSQL.35432")?)?;
    require(
        socket.file_type().is_socket()
            && socket.uid() == uid
            && socket.gid() == gid
            && socket.mode() & 0o7777 == mode,
        "PostgreSQL socket ownership or mode",
    )
}

pub(super) fn read_file(path: &Path, limit: u64) -> R<(Vec<u8>, fs::Metadata)> {
    let (mut file, meta) = td_fs::open_real_file(path, "production file")?;
    require(
        meta.len() <= limit && meta.nlink() == 1,
        "unexpected production file size or links",
    )?;
    let mut bytes = td_alloc::filled(usize::try_from(meta.len())?, 0)?;
    file.read_exact(&mut bytes)?;
    require(file.read(&mut [0])? == 0, "production file changed")?;
    Ok((bytes, meta))
}

impl Deployment {
    // The settings as the server reads them, postgresql.auto.conf included,
    // with no fence file present.
    fn socket_settings(&self) -> R<()> {
        for &(setting, expected) in SOCKET_SETTINGS {
            let mut cmd = self.host.cmd("runuser");
            cmd.args(["-u", "td-mta", "--"])
                .arg(self.host.tool("postgres")?)
                .args(["-D", PGDATA, "-c"])
                .arg(fmt(format_args!("config_file={MAIL}/postgresql.conf"))?)
                .args(["-C", setting]);
            require(
                self.host.out(&mut cmd)? == expected,
                "PostgreSQL socket setting",
            )?;
        }
        Ok(())
    }

    /// The private socket state every activation expects.
    pub(super) fn socket_preflight(&self) -> R<()> {
        let gid = isolation::db_group(&self.host)?;
        let uid = fs::metadata(PGDATA)?.uid();
        let dropin = Setting {
            path: PathBuf::from(PG_DROPIN),
            text: owned(PG_DROPIN_TEXT)?,
            mode: 0o644,
        };
        require(
            files::present(&dropin)?,
            "missing PostgreSQL socket group setting",
        )?;
        require(
            !Path::new(PG_FENCE).try_exists()?,
            "stale PostgreSQL socket fence: run td-mta/deploy --recover",
        )?;
        fence::fence_directory(false)?;
        self.socket_settings()?;
        files::safe_parents(Path::new(SOCKET_DIR))?;
        check_socket(Path::new(SOCKET_DIR), uid, gid, 0o660)
    }
}

#[cfg(test)]
#[path = "tests/postgres_socket_pg.rs"]
mod postgres_test;
