//! Exercise signed acquisition before the existing guest install/rollback oracle.
use super::{Path, PathBuf, RecipeCheckRunner};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "../../../../../../td-fs/src/temp_dir.rs"]
#[allow(dead_code)] // This host-only oracle uses the guard's Drop cleanup.
pub(super) mod fixture_temp;

const COMMIT: &str = "ffffffffffffffffffffffffffffffffffffffff";

pub(super) struct Delivery {
    program: PathBuf,
    work: PathBuf,
    sequence: u64,
    profile: &'static str,
    _owned: fixture_temp::TempDir,
}

impl Delivery {
    pub(super) fn new(runner: &RecipeCheckRunner, profile: &'static str) -> Result<Self, String> {
        runner.prepare_recipe_target("td-net")?;
        let built = runner.build_plan("td-net")?;
        let program = runner.ladder_out_from(&built, "td-net")?.join("bin/td-net");
        let owned = fixture_temp::TempDir::new("td-signed-release-oracle")
            .map_err(|e| format!("create signed delivery fixture: {e}"))?;
        let work = owned.path().to_path_buf();
        let delivery = Self {
            program,
            work,
            sequence: 0,
            profile,
            _owned: owned,
        };
        delivery.command(&[
            "keygen",
            &delivery.path("root.key")?,
            &delivery.path("root.pub")?,
        ])?;
        delivery.command(&[
            "keygen",
            &delivery.path("release.key")?,
            &delivery.path("release.pub")?,
        ])?;
        let expires = (now()? + 86400).to_string();
        delivery.command(&[
            "root",
            "1",
            &expires,
            &delivery.path("root.key")?,
            &delivery.path("release.pub")?,
            &delivery.path("root.key")?,
            &delivery.path("root")?,
        ])?;
        for state in ["worker-state", "verifier-state"] {
            delivery.command(&[
                "init",
                &delivery.path(state)?,
                &delivery.path("root")?,
                profile,
            ])?;
        }
        Ok(delivery)
    }

    pub(super) fn path(&self, name: &str) -> Result<String, String> {
        self.work
            .join(name)
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| "non-UTF-8 release fixture path".into())
    }

    pub(super) fn command(&self, args: &[&str]) -> Result<String, String> {
        let output = Command::new(&self.program)
            .arg("release")
            .args(args)
            .env_clear()
            .current_dir(&self.work)
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .map_err(|e| format!("start release fixture command: {e}"))?;
        if !output.status.success() || output.stdout.len() > 4096 {
            return Err(format!("release fixture command failed: {}", output.status));
        }
        String::from_utf8(output.stdout).map_err(|_| "release fixture output is not UTF-8".into())
    }

    pub(super) fn materialize(&mut self, candidate: &Path) -> Result<(), String> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("release fixture sequence overflow")?;
        let sequence = self.sequence.to_string();
        let issued = now()?;
        let expires = (issued + 7200).to_string();
        let issued = issued.to_string();
        let candidate_text = candidate.to_str().ok_or("non-UTF-8 candidate path")?;
        let id = crate::sha256::sha256_file(&candidate.join("manifest"))
            .map_err(|e| format!("hash release fixture manifest: {e}"))?;
        let index = self.command(&[
            "publish",
            &self.path("repo")?,
            &self.path("root")?,
            &self.path("release.key")?,
            self.profile,
            COMMIT,
            &sequence,
            &issued,
            &expires,
            candidate_text,
        ])?;
        let index = index
            .strip_suffix('\n')
            .ok_or("missing published index receipt")?;
        if !td_recipe::td_boot_protocol::valid_digest(index.as_bytes()) {
            return Err("invalid published index receipt".into());
        }
        self.command(&[
            "promote",
            &self.path("repo")?,
            &self.path("root")?,
            &self.path("release.key")?,
            self.profile,
            index,
            &sequence,
            &issued,
            &expires,
        ])?;
        // The caller owns this seed tree. Stage beside the candidate so rename
        // remains on one filesystem even when TMPDIR is a separate mount.
        let parent = candidate.parent().ok_or("candidate has no parent")?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("protect signed download parent: {e}"))?;
        let output_path = parent.join(format!(".signed-download-{}", self.sequence));
        let output = output_path
            .to_str()
            .ok_or("non-UTF-8 signed download path")?;
        self.command(&[
            "fetch",
            &format!("file:{}", self.path("repo")?),
            &self.path("worker-state")?,
            &self.path("cache")?,
            output,
        ])?;
        self.command(&[
            "learn",
            &self.path("verifier-state")?,
            &self.path("worker-state.metadata")?,
        ])?;
        let verified = self.command(&["verify", &self.path("verifier-state")?, output])?;
        if verified != format!("{COMMIT} {id}\n") {
            return Err("signed delivery changed the fixture deployment".into());
        }
        fs::remove_dir_all(candidate).map_err(|e| format!("remove pre-delivery fixture: {e}"))?;
        fs::rename(output, candidate)
            .map_err(|e| format!("stage verified downloaded fixture: {e}"))?;
        println!(
            "[signed-release] {} release {} acquired and independently verified: {id}",
            self.profile, self.sequence
        );
        Ok(())
    }
}

fn now() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .map_err(|e| e.to_string())
}
