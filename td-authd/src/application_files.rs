//! Root publication of the declared human/application filesystem views.

use crate::{application, launch, mount_sys, portal_files};
use std::fs::{self, File};
use std::os::fd::AsFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Clone, Copy)]
enum Grant {
    FirefoxDownloads,
    MailDownloads,
    Workspace,
}

impl Grant {
    fn parse(arguments: &[String]) -> Result<Self, String> {
        match arguments {
            [name] if name == "firefox" => Ok(Self::FirefoxDownloads),
            [name] if name == "mail" => Ok(Self::MailDownloads),
            [name] if name == "claude" => Ok(Self::Workspace),
            _ => Err("application files require exactly firefox, mail or claude".into()),
        }
    }

    fn application(self) -> &'static str {
        match self {
            Self::FirefoxDownloads => "firefox",
            Self::MailDownloads => "mail",
            Self::Workspace => "claude",
        }
    }

    fn component(self) -> &'static str {
        match self {
            Self::FirefoxDownloads | Self::MailDownloads => "Downloads",
            Self::Workspace => "src",
        }
    }

    fn view(self, uid: u32) -> String {
        format!("/var/lib/td/applications/{uid}/{}", self.component())
    }
}

fn require_options(options: &str) -> Result<(), String> {
    for option in ["rw", "nosuid", "nodev", "noexec"] {
        if !options.split(',').any(|value| value == option) {
            return Err(format!("application file grant lacks {option}"));
        }
    }
    Ok(())
}

fn require_same_directory(source: &fs::Metadata, view: &fs::Metadata) -> Result<(), String> {
    if source.dev() != view.dev() || source.ino() != view.ino() {
        return Err("application view no longer names its declared human directory".into());
    }
    Ok(())
}

fn require_view(
    grant: Grant,
    uid: u32,
    source: &File,
    home: &File,
    options: &str,
) -> Result<(), String> {
    require_options(options)?;
    let source = source.metadata().map_err(|e| e.to_string())?;
    let view = portal_files::child(home, grant.component(), uid, false)?;
    require_same_directory(&source, &view.metadata().map_err(|e| e.to_string())?)
}

pub(crate) fn prepare(arguments: &[String]) -> Result<(), String> {
    let grant = Grant::parse(arguments)?;
    launch::require_launch_startup()?;
    let uid = application::admitted_uid(grant.application())?;
    let root = portal_files::directory(Path::new("/"), 0, true)?;
    let var = portal_files::child(&root, "var", 0, true)?;
    let homes = portal_files::child(&var, "home", 0, true)?;
    let human = portal_files::child(&homes, "tester", 1000, true)?;
    let source_path = format!("/proc/self/fd/{}/{}",
        std::os::fd::AsRawFd::as_raw_fd(&human), grant.component());
    match fs::DirBuilder::new().mode(0o700).create(&source_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("create declared human directory: {error}")),
    }
    let metadata = fs::symlink_metadata(&source_path).map_err(|e| e.to_string())?;
    if metadata.uid() == 0 {
        let created = portal_files::child(&human, grant.component(), 0, true)?;
        let mode = created.metadata().map_err(|e| e.to_string())?.mode() & 0o7777;
        if mode & !0o700 != 0 || fs::read_dir(format!("/proc/self/fd/{}",
            std::os::fd::AsRawFd::as_raw_fd(&created)))
            .map_err(|e| e.to_string())?.next().is_some() {
            return Err("interrupted human grant directory must be empty and private".into());
        }
        created.set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        std::os::unix::fs::fchown(&created, Some(1000), Some(1000))
            .map_err(|e| e.to_string())?;
    }
    let source = portal_files::child(&human, grant.component(), 1000, true)?;
    if source.metadata().map_err(|e| e.to_string())?.mode() & 0o7777 != 0o700 {
        return Err(format!(
            "{} startup refused: human {} must have mode 0700; its owner must restore private permissions",
            grant.application(), grant.component(),
        ));
    }
    let lib = portal_files::child(&var, "lib", 0, true)?;
    let td = portal_files::child(&lib, "td", 0, true)?;
    let applications = portal_files::child(&td, "applications", 0, true)?;
    let home = portal_files::child(&applications, &uid.to_string(), uid, true)?;
    if home.metadata().map_err(|e| e.to_string())?.mode() & 0o7777 != 0o700 {
        return Err("application file-grant home must have mode 0700".into());
    }
    let path = grant.view(uid);
    if let Some(options) = portal_files::mount_options_at(&path)? {
        return require_view(grant, uid, &source, &home, &options);
    }
    let target = portal_files::ensure_root_child(&home, grant.component())?;
    if fs::read_dir(format!(
        "/proc/self/fd/{}",
        std::os::fd::AsRawFd::as_raw_fd(&target)
    ))
    .map_err(|e| e.to_string())?
    .next()
    .is_some()
    {
        return Err("unpublished application file-grant target is not empty".into());
    }
    let namespace = portal_files::namespace_for(uid)?;
    let mount = mount_sys::clone_directory(source.as_fd()).map_err(|e| e.to_string())?;
    mount_sys::application_attributes(mount.as_fd(), namespace.as_fd())
        .map_err(|e| e.to_string())?;
    mount_sys::publish(mount.as_fd(), target.as_fd()).map_err(|e| e.to_string())?;
    let options =
        portal_files::mount_options_at(&path)?.ok_or("application file grant was not published")?;
    require_view(grant, uid, &source, &home, &options)
}

pub(crate) fn release(arguments: &[String]) -> Result<(), String> {
    let grant = Grant::parse(arguments)?;
    launch::require_launch_startup()?;
    let uid = application::admitted_uid(grant.application())?;
    let path = grant.view(uid);
    if portal_files::mount_options_at(&path)?.is_none() {
        return Ok(());
    }
    let status = Command::new("/bin/umount")
        .arg(&path)
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("unmount application file grant: {e}"))?;
    if !status.success() || portal_files::mount_options_at(&path)?.is_some() {
        return Err("application file-grant unmount did not complete".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    #[test]
    fn existing_view_requires_all_mount_restrictions_and_the_same_inode() {
        use super::*;
        let root = std::env::temp_dir().join(format!("td-application-view-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let result = std::panic::catch_unwind(|| {
            fs::create_dir(root.join("Downloads")).unwrap();
            fs::create_dir(root.join("different")).unwrap();
            let source = fs::metadata(root.join("Downloads")).unwrap();
            let same = fs::metadata(root.join("Downloads")).unwrap();
            let different = fs::metadata(root.join("different")).unwrap();
            let required = ["rw", "nosuid", "nodev", "noexec"];
            assert!(require_options(&required.join(",")).is_ok());
            assert!(require_options("nodev,noexec,rw,nosuid,relatime").is_ok());
            for absent in required {
                let options = required.iter().copied().filter(|value| *value != absent)
                    .collect::<Vec<_>>().join(",");
                assert!(require_options(&options).is_err());
                assert!(require_options(&format!("{options},{absent}-suffix")).is_err());
            }
            assert!(require_same_directory(&source, &same).is_ok());
            assert!(require_same_directory(&source, &different).is_err());
        });
        fs::remove_dir_all(root).unwrap();
        assert!(result.is_ok());
    }
    #[test]
    fn grants_accept_only_the_three_fixed_installed_names() {
        for name in ["firefox", "mail", "claude"] {
            assert!(super::Grant::parse(&[name.into()]).is_ok());
        }
        for arguments in [
            vec![],
            vec!["news".into()],
            vec!["../firefox".into()],
            vec!["firefox".into(), "/tmp".into()],
        ] {
            assert!(super::Grant::parse(&arguments).is_err());
        }
    }
}
