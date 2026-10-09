//! Real workspace/jail, offline model exchanges, and CLI lifecycle checks.
#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

#[path = "support/mock_fetch.rs"]
#[allow(dead_code)]
mod mock_fetch;

use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mock_fetch::{MockFetch, Reply};
use td_json::Json;

const MODEL: &str = "meta-llama/llama-4-small";
const PROGRAM: &str = env!("CARGO_BIN_EXE_td-agent");

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    config: PathBuf,
    commit: String,
}

impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "review-{}",
            td_agent::store::random_hex(8).unwrap()
        ));
        fs::create_dir_all(root.parent().unwrap()).unwrap();
        DirBuilder::new().mode(0o700).create(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let source = root.join("input");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("source-only"), "ORIGINAL\n").unwrap();
        fs::write(source.join("Cargo.toml"), "[package]\nname = \"review-process-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"fixture.rs\"\n").unwrap();
        fs::write(
            source.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"review-process-fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            source.join("fixture.rs"),
            "#[test]\nfn fixture_passes() { assert_eq!(2 + 2, 4); }\n",
        )
        .unwrap();
        fs::create_dir(source.join("dep")).unwrap();
        fs::write(source.join("dep/data"), "DEPENDENCY\n").unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(&source)
                .args(args)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "Review fixture")
                .env("GIT_AUTHOR_EMAIL", "review@example.invalid")
                .env("GIT_COMMITTER_NAME", "Review fixture")
                .env("GIT_COMMITTER_EMAIL", "review@example.invalid")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        };
        git(&["init", "--quiet"]);
        git(&["add", "."]);
        git(&["commit", "--quiet", "-m", "fixture base"]);
        fs::write(source.join("source-only"), "ORIGINAL REVIEWED\n").unwrap();
        git(&["add", "source-only"]);
        git(&["commit", "--quiet", "-m", "fixture review"]);
        let commit = git(&["rev-parse", "HEAD"]).trim().to_string();
        let config = root.join("config");
        DirBuilder::new().mode(0o700).create(&config).unwrap();
        DirBuilder::new()
            .mode(0o700)
            .create(config.join("td-agent"))
            .unwrap();
        let key = config.join("td-agent/openrouter.key");
        fs::write(&key, "sk-or-v1-fixture-not-a-real-key\n").unwrap();
        fs::set_permissions(key, fs::Permissions::from_mode(0o600)).unwrap();
        Self {
            root,
            source,
            config,
            commit,
        }
    }

    fn command(&self, mock: &MockFetch, cap: &str) -> Command {
        let mut cmd = Command::new(PROGRAM);
        cmd.args(["review", "--repo"])
            .arg(&self.source)
            .args(["--model", MODEL, "--max-tokens", "4096", "--max-cost", cap])
            .env("XDG_CONFIG_HOME", &self.config)
            .env("XDG_RUNTIME_DIR", mock.runtime())
            .env("TD_AGENT_TEST_KEY_ROOT", &self.root)
            .stdin(Stdio::null());
        cmd
    }

    fn no_workspaces(&self) {
        let root = self.config.join("td-agent/reviews");
        assert!(fs::read_dir(root).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("run-")));
        assert_eq!(
            fs::read_to_string(self.source.join("source-only")).unwrap(),
            "ORIGINAL REVIEWED\n"
        );
    }

    fn final_reply(&self) -> Reply {
        completion(
            &format!(
                "REVIEWING: fixture review ({})\nNo findings. Tool checks completed.",
                self.commit
            ),
            None,
            "stop",
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = td_agent::workspace::remove_tree(&self.root);
    }
}

fn models() -> Reply {
    Reply::Http {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: mock_fetch::fixture("models.json"),
    }
}

fn completion(content: &str, tool: Option<(&str, &str)>, finish: &str) -> Reply {
    let mut message = vec![("content".into(), Json::Str(content.into()))];
    if let Some((name, args)) = tool {
        message.push((
            "tool_calls".into(),
            Json::Arr(vec![Json::Obj(vec![
                ("id".into(), Json::Str("call-1".into())),
                ("type".into(), Json::Str("function".into())),
                (
                    "function".into(),
                    Json::Obj(vec![
                        ("name".into(), Json::Str(name.into())),
                        ("arguments".into(), Json::Str(args.into())),
                    ]),
                ),
            ])]),
        ));
    }
    let body = Json::Obj(vec![
        ("model".into(), Json::Str(MODEL.into())),
        ("provider".into(), Json::Str("Offline fixture".into())),
        (
            "choices".into(),
            Json::Arr(vec![Json::Obj(vec![
                ("message".into(), Json::Obj(message)),
                ("finish_reason".into(), Json::Str(finish.into())),
            ])]),
        ),
        (
            "usage".into(),
            Json::Obj(vec![
                ("prompt_tokens".into(), Json::from(100u64)),
                ("completion_tokens".into(), Json::from(10u64)),
                ("cost".into(), Json::Num("0.000001".into())),
            ]),
        ),
    ])
    .to_string()
    .into_bytes();
    Reply::Http {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body,
    }
}

fn tool(name: &str, args: Json) -> Reply {
    completion("", Some((name, &args.to_string())), "tool_calls")
}

fn run(fixture: &Fixture, mock: &MockFetch, cap: &str) -> std::process::Output {
    let mut child = fixture
        .command(mock, cap)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("review did not exit within 60 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn tools_read_only_source_write_scratch_expand_sparse_and_cleanup() {
    let fixture = Fixture::new();
    let args = Json::Obj(vec![("command".into(), Json::Str("if printf BAD > source-only; then echo SOURCE_WRITABLE; else echo SOURCE_READ_ONLY; fi; cat source-only; mkdir -p \"$CARGO_TARGET_DIR\"; printf SCRATCH_OK > \"$CARGO_TARGET_DIR/marker\"; cat \"$CARGO_TARGET_DIR/marker\"".into()))]);
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![
            models(),
            tool("shell", args),
            tool(
                "expand_sparse",
                Json::Obj(vec![(
                    "paths".into(),
                    Json::Arr(vec![Json::Str("dep".into())]),
                )]),
            ),
            tool(
                "shell",
                Json::Obj(vec![("command".into(), Json::Str("cat dep/data".into()))]),
            ),
            fixture.final_reply(),
        ],
    );
    let result = run(&fixture, &mock, "0.02");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    assert!(requests[2].text().contains("SOURCE_READ_ONLY"));
    assert!(requests[2].text().contains("ORIGINAL REVIEWED"));
    assert!(requests[2].text().contains("SCRATCH_OK"));
    assert!(requests[4].text().contains("DEPENDENCY"));
    assert!(!requests[1].text().contains("\"name\":\"git_push\""));
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn corrupt_owner_link_is_reported_without_blocking_new_reviews() {
    let fixture = Fixture::new();
    let base = fixture.config.join("td-agent/reviews");
    DirBuilder::new().mode(0o700).create(&base).unwrap();
    let corrupt = base.join("run-corrupt");
    DirBuilder::new().mode(0o700).create(&corrupt).unwrap();
    let key = fixture.config.join("td-agent/openrouter.key");
    std::os::unix::fs::symlink(&key, corrupt.join("owner.lock")).unwrap();
    let original = fs::read(&key).unwrap();
    let mock = MockFetch::start(&fixture.root.join("run"), vec![models()]);
    let result = run(&fixture, &mock, "0");
    assert!(!result.status.success());
    assert_eq!(mock.requests().len(), 1);
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(error.contains("cannot collect"), "{error}");
    assert!(error.contains("--max-cost"), "{error}");
    assert_eq!(fs::read(&key).unwrap(), original);
    fs::remove_file(corrupt.join("owner.lock")).unwrap();
    fs::remove_dir(corrupt).unwrap();
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn cargo_runs_offline_with_build_outputs_in_scratch() {
    let fixture = Fixture::new();
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![models(), tool("shell", Json::Obj(vec![("command".into(), Json::Str("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=gcc cargo test --locked --offline".into()))])), fixture.final_reply()],
    );
    let result = run(&fixture, &mock, "0.02");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let requests = mock.requests();
    assert!(
        requests[2].text().contains("1 passed; 0 failed"),
        "{}",
        requests[2].text()
    );
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn wrong_commit_identity_never_reaches_stdout() {
    let fixture = Fixture::new();
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![
            models(),
            completion(
                "REVIEWING: another commit (wrong)\nNo defects.",
                None,
                "stop",
            ),
        ],
    );
    let result = run(&fixture, &mock, "0.02");
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("exact commit"));
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn zero_cap_sends_no_completion_and_cleanup_runs() {
    let fixture = Fixture::new();
    let mock = MockFetch::start(&fixture.root.join("run"), vec![models()]);
    let result = run(&fixture, &mock, "0");
    assert!(!result.status.success());
    assert_eq!(mock.requests().len(), 1);
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn forbidden_tools_and_cut_reviews_cannot_be_accepted() {
    let fixture = Fixture::new();
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![
            models(),
            tool("write_file", Json::Obj(Vec::new())),
            completion("partial", None, "length"),
        ],
    );
    let result = run(&fixture, &mock, "0.02");
    assert!(!result.status.success());
    assert!(mock.requests()[2].text().contains("no tool named"));
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn killed_review_is_collected_on_the_next_invocation() {
    let fixture = Fixture::new();
    let mock = MockFetch::start(&fixture.root.join("run"), vec![models(), Reply::Hang]);
    let mut child = fixture
        .command(&mock, "0.02")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    mock.wait_for(2);
    child.kill().unwrap();
    child.wait().unwrap();
    mock.then(vec![models()]);
    let result = run(&fixture, &mock, "0");
    assert!(!result.status.success());
    fixture.no_workspaces();
}
