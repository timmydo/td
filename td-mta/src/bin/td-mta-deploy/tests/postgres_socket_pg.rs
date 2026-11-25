//! The database writer fence on a disposable PostgreSQL 17 cluster: the
//! included file decides the mode a start gives the socket, and the live
//! mode decides who opens a new connection.
use crate::{
    activate::fence::{close_socket, open_socket, SocketFence, FENCE_SETTING},
    postgres_fixture::{checked, run, wait, PgTools, Process},
    R,
};
use std::{
    fs, io,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Output,
};

struct Fixture {
    fence: PathBuf,
    socket: PathBuf,
}

impl SocketFence for Fixture {
    fn write_file(&mut self) -> R<()> {
        fs::write(&self.fence, FENCE_SETTING)?;
        Ok(())
    }
    fn remove_file(&mut self) -> R<()> {
        match fs::remove_file(&self.fence) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }
    fn set_mode(&mut self, mode: u32) -> R<()> {
        if self.socket.exists() {
            fs::set_permissions(&self.socket, fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    }
}

#[test]
#[ignore = "requires PostgreSQL 17 tools as a non-root user"]
fn real_postgres_fence_file_and_live_mode_close_new_connections() -> R<()> {
    let tools = PgTools::new()?;
    // Keep the socket address below Linux's sockaddr_un limit.
    let directory = td_fs::TempDir::new_in(Path::new("/tmp"), "tdfence")?;
    let root = directory.path();
    let data = root.join("data");
    let sockets = root.join("s");
    fs::create_dir(&sockets)?;
    fs::set_permissions(&sockets, fs::Permissions::from_mode(0o750))?;
    checked(tools.cmd("initdb").arg("-D").arg(&data).args([
        "--no-sync",
        "--no-locale",
        "-U",
        "fixture",
        "--auth-local=trust",
        "--auth-host=reject",
    ]))?;
    let mut fixture = Fixture {
        fence: root.join("fence.conf"),
        socket: sockets.join(".s.PGSQL.35432"),
    };
    // The test account stands in for the socket group it is a member of.
    let group = fs::metadata(root)?.gid();
    // Production's socket settings, the fence include last.
    let conf = root.join("postgresql.conf");
    fs::write(
        &conf,
        format!(
            "port = 35432\nmax_connections = 20\nlisten_addresses = ''\nssl = off\n\
             unix_socket_directories = '{}'\nunix_socket_group = '{group}'\n\
             unix_socket_permissions = 0660\ninclude_if_exists = '{}'\n",
            sockets.display(),
            fixture.fence.display(),
        ),
    )?;
    let start = |log: &str| -> R<Process> {
        let log = root.join(log);
        let server = run(
            tools
                .cmd("postgres")
                .arg("-D")
                .arg(&data)
                .arg("-c")
                .arg(format!("config_file={}", conf.display())),
            &log,
        )?;
        wait("server startup", || {
            fs::read_to_string(&log)
                .is_ok_and(|text| text.contains("database system is ready to accept connections"))
        })?;
        Ok(server)
    };
    let connect = || -> R<Output> {
        Ok(tools
            .cmd("psql")
            .args(["-XwqAt", "-h"])
            .arg(&sockets)
            .args([
                "-p", "35432", "-U", "fixture", "-d", "postgres", "-c", "SELECT 1",
            ])
            .output()?)
    };
    let refused = |output: Output| {
        assert!(!output.status.success(), "connection unexpectedly opened");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Permission denied"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let socket = |fixture: &Fixture| -> R<(u32, u32)> {
        let meta = fs::symlink_metadata(&fixture.socket)?;
        Ok((meta.mode() & 0o7777, meta.gid()))
    };

    // A start while the file exists creates the socket closed.
    close_socket(&mut fixture)?;
    {
        let _server = start("fenced.log")?;
        assert_eq!(socket(&fixture)?, (0, group));
        // The fifth line names the running postmaster's first socket.
        let pid = fs::read_to_string(data.join("postmaster.pid"))?;
        assert_eq!(pid.lines().nth(4), sockets.to_str());
        refused(connect()?);
    }
    // With the file removed first, the next start opens it.
    open_socket(&mut fixture)?;
    let _server = start("open.log")?;
    assert_eq!(socket(&fixture)?, (0o660, group));
    let opened = connect()?;
    assert!(opened.status.success());
    assert_eq!(String::from_utf8_lossy(&opened.stdout).trim(), "1");
    // Closing the live socket refuses a new connection without a restart.
    close_socket(&mut fixture)?;
    assert_eq!(socket(&fixture)?.0, 0);
    assert_eq!(fs::read_to_string(&fixture.fence)?, FENCE_SETTING);
    refused(connect()?);
    open_socket(&mut fixture)?;
    assert!(!fixture.fence.exists());
    assert!(connect()?.status.success());
    Ok(())
}
