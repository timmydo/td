use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

pub fn remove_stale(path: &Path, kind: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => match UnixStream::connect(path) {
            Ok(_) => Err(format!(
                "refusing to replace live {kind} socket {}",
                path.display()
            )),
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                fs::remove_file(path)
                    .map_err(|e| format!("remove stale {kind} socket {}: {e}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "probe existing {kind} socket {}: {error}",
                path.display()
            )),
        },
        Ok(_) => Err(format!(
            "refusing to replace non-socket {kind} path {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("stat {kind} socket {}: {error}", path.display())),
    }
}
