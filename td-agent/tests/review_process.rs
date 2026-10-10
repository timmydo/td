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
use std::io::Write;
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
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", mock.runtime())
            .env("TD_AGENT_TEST_KEY_ROOT", &self.root)
            .stdin(Stdio::null());
        cmd
    }

    fn logs(&self) -> Vec<String> {
        fs::read_dir(self.root.join("state/td-agent/reviews"))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                assert_eq!(
                    fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
                let text = fs::read_to_string(path).unwrap();
                assert!(!text.contains("sk-or-v1-fixture-not-a-real-key"));
                for (sequence, line) in text.lines().enumerate() {
                    let event = td_json::parse(line).unwrap();
                    assert_eq!(
                        event.get("sequence").and_then(Json::as_u64),
                        Some(sequence as u64)
                    );
                }
                text
            })
            .collect()
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
fn provider_routing_prices_selected_endpoints_and_keeps_the_session_across_turns() {
    let fixture = Fixture::new();
    let endpoints = Reply::Http { status: 200, headers: vec![], body: format!(r#"{{"data":{{"id":"{MODEL}","endpoints":[{{"tag":"cheap/fp8","context_length":1000000,"max_completion_tokens":32768,"supported_parameters":["tools","max_tokens"],"pricing":{{"prompt":"0.00000001","completion":"0.00000001"}}}},{{"tag":"dear","context_length":1000000,"supported_parameters":["tools","max_tokens"],"pricing":{{"prompt":"0.01","completion":"0.01"}}}}]}}}}"#).into_bytes() };
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![
            endpoints,
            tool(
                "read_file",
                Json::Obj(vec![(
                    "path".into(),
                    Json::Str("/source/source-only".into()),
                )]),
            ),
            fixture.final_reply(),
        ],
    );
    let result = fixture
        .command(&mock, "0.02")
        .args([
            "--routing",
            "floor",
            "--provider",
            "cheap",
            "--no-provider-fallbacks",
            "--max-input-price",
            "0.02",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].url.ends_with("/endpoints"));
    let first = td_json::parse(&requests[1].text()).unwrap();
    let next = td_json::parse(&requests[2].text()).unwrap();
    assert_eq!(
        first.get("model").and_then(Json::as_str),
        Some(format!("{MODEL}:floor").as_str())
    );
    assert_eq!(
        first.get_path(&["provider", "only"]),
        Some(&Json::Arr(vec![Json::Str("cheap".into())]))
    );
    assert_eq!(
        first.get_path(&["provider", "allow_fallbacks"]),
        Some(&Json::Bool(false))
    );
    assert_eq!(
        first
            .get_path(&["provider", "max_price", "prompt"])
            .and_then(Json::as_f64),
        Some(0.01)
    );
    assert_eq!(first.get("session_id"), next.get("session_id"));
    let logs = fixture.logs();
    assert!(logs[0].contains("routing_endpoints"));
    assert!(logs[0].contains(first.get("session_id").unwrap().as_str().unwrap()));
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn diff_review_routes_and_explains_default_price_ceiling_refusals() {
    for routed in [false, true] {
        let fixture = Fixture::new();
        let first = if routed {
            Reply::Http {status:200,headers:vec![],body:format!(r#"{{"data":{{"id":"{MODEL}","endpoints":[{{"tag":"cheap","supported_parameters":["max_tokens"],"pricing":{{"prompt":"0.00000001","completion":"0.00000001"}}}}]}}}}"#).into_bytes()}
        } else {
            models()
        };
        let mock = MockFetch::start(
            &fixture.root.join("run"),
            vec![
                first,
                Reply::Http {
                    status: 404,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: br#"{"error":{"message":"No endpoints found"}}"#.to_vec(),
                },
            ],
        );
        let input = fixture.root.join("commit.txt");
        fs::write(&input, "commit 1234567890\n\n    fixture review\n").unwrap();
        let template = fixture.command(&mock, "0.02");
        let mut command = Command::new(PROGRAM);
        command.args([
            "review",
            "--model",
            MODEL,
            "--max-tokens",
            "4096",
            "--max-cost",
            "0.02",
        ]);
        if routed {
            command.args(["--routing", "fastest", "--provider", "cheap"]);
        }
        command.arg(input).stdin(Stdio::null());
        for (name, value) in template.get_envs() {
            if let Some(value) = value {
                command.env(name, value);
            }
        }
        let result = command.output().unwrap();
        assert!(!result.status.success());
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            stderr.contains("review provider price ceiling")
                && stderr.contains("--routing balanced"),
            "{stderr}"
        );
        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        let body = td_json::parse(&requests[1].text()).unwrap();
        assert!(body.get_path(&["provider", "max_price"]).is_some());
        assert_eq!(
            body.get_path(&["provider", "sort"]).and_then(Json::as_str),
            if routed { Some("throughput") } else { None }
        );
        assert!(!fixture.config.join("td-agent/reviews").exists());
    }
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
            tool(
                "shell",
                Json::Obj(vec![(
                    "command".into(),
                    Json::Str(format!(
                        "if test -e '{}'; then echo LOGS_VISIBLE; else echo LOGS_PRIVATE; fi",
                        fixture
                            .root
                            .join("state/td-agent/reviews")
                            .display()
                            .to_string()
                            .replace('\'', "'\\''")
                    )),
                )]),
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
    assert_eq!(requests.len(), 6);
    assert!(requests[2].text().contains("SOURCE_READ_ONLY"));
    assert!(requests[2].text().contains("ORIGINAL REVIEWED"));
    assert!(requests[2].text().contains("SCRATCH_OK"));
    assert!(requests[4].text().contains("DEPENDENCY"));
    assert!(requests[5].text().contains("LOGS_PRIVATE"));
    assert!(!requests[1].text().contains("\"name\":\"git_push\""));
    fixture.no_workspaces();
    let logs = fixture.logs();
    assert_eq!(logs.len(), 1);
    let trace = &logs[0];
    for kind in [
        "request_body",
        "response_bytes_hex",
        "tool_call",
        "tool_result",
        "budget_settlement",
        "jail_spec",
        "cleanup",
        "end",
    ] {
        assert!(
            trace.contains(&format!("\"kind\":\"{kind}\"")),
            "missing {kind}"
        );
    }
    assert!(trace.contains("SOURCE_READ_ONLY"));
    assert!(trace.contains("DEPENDENCY"));
    assert!(trace.lines().last().unwrap().contains("\"success\":true"));
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("session log "));
}

/// A repository staged as `/review` stages one: bare, holding only the
/// reviewed commit's own objects and borrowing the rest from a store
/// through its alternates. Its checkout and its tools reach what only
/// the store holds.
#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn a_staged_repository_borrows_its_stores_objects() {
    let fixture = Fixture::new();
    let git = |dir: &Path, args: &[&str], input: Option<&[u8]>| {
        let mut child = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.unwrap_or_default()).unwrap();
        drop(stdin);
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    };
    let source = fixture.source.clone();
    let store = fixture.root.join("store.git");
    let staged = fixture.root.join("review.git");
    git(
        &fixture.root,
        &["init", "--quiet", "--bare", "store.git"],
        None,
    );
    git(
        &source,
        &[
            "push",
            "--quiet",
            store.to_str().unwrap(),
            "HEAD~1:refs/heads/main",
        ],
        None,
    );
    git(
        &fixture.root,
        &["init", "--quiet", "--bare", "review.git"],
        None,
    );
    fs::write(
        staged.join("objects/info/alternates"),
        format!("{}\n", store.join("objects").display()),
    )
    .unwrap();
    let pack = git(
        &source,
        &["pack-objects", "--stdout", "--revs", "--quiet"],
        Some(format!("{}\n^HEAD~1\n", fixture.commit).as_bytes()),
    );
    git(&staged, &["index-pack", "--strict", "--stdin"], Some(&pack));
    // The base's file is the store's alone.
    let blob = String::from_utf8(git(&source, &["rev-parse", "HEAD:dep/data"], None)).unwrap();
    assert!(!staged
        .join("objects")
        .join(&blob[..2])
        .join(blob[2..].trim())
        .exists());
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![
            models(),
            tool(
                "expand_sparse",
                Json::Obj(vec![(
                    "paths".into(),
                    Json::Arr(vec![Json::Str("dep".into())]),
                )]),
            ),
            tool(
                "shell",
                Json::Obj(vec![(
                    "command".into(),
                    Json::Str("cat dep/data source-only; git log --format=%s".into()),
                )]),
            ),
            fixture.final_reply(),
        ],
    );
    let result = Command::new(PROGRAM)
        .args(["review", "--repo"])
        .arg(&staged)
        .args(["--commit", &fixture.commit])
        .args([
            "--model",
            MODEL,
            "--max-tokens",
            "4096",
            "--max-cost",
            "0.02",
        ])
        .env("XDG_CONFIG_HOME", &fixture.config)
        .env("XDG_STATE_HOME", fixture.root.join("state"))
        .env("XDG_RUNTIME_DIR", mock.runtime())
        .env("TD_AGENT_TEST_KEY_ROOT", &fixture.root)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    let said = requests[3].text();
    assert!(said.contains("DEPENDENCY"), "{said}");
    assert!(said.contains("ORIGINAL REVIEWED"), "{said}");
    assert!(said.contains("fixture base"), "{said}");
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn full_tool_output_survives_model_truncation() {
    let fixture = Fixture::new();
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![
            models(),
            tool(
                "shell",
                Json::Obj(vec![(
                    "command".into(),
                    Json::Str(
                        "printf '%080000d' 0; printf TRACE_MIDDLE; printf '%080000d' 0".into(),
                    ),
                )]),
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
    let body = td_json::parse(&requests[2].text()).unwrap();
    let messages = body.get("messages").and_then(Json::as_arr).unwrap();
    let result = messages
        .iter()
        .rev()
        .find(|message| message.get("role").and_then(Json::as_str) == Some("tool"))
        .unwrap()
        .get("content")
        .and_then(Json::as_str)
        .unwrap();
    assert!(!result.contains("TRACE_MIDDLE"));
    assert!(result.contains("output shortened"));
    assert!(result.len() <= 8192);
    let logs = fixture.logs();
    let hex: String = logs[0]
        .lines()
        .filter_map(|line| {
            let event = td_json::parse(line).unwrap();
            (event.get("kind").and_then(Json::as_str) == Some("tool_output_bytes_hex")).then(|| {
                event
                    .get("data")
                    .and_then(Json::as_str)
                    .unwrap()
                    .to_string()
            })
        })
        .collect();
    let output: Vec<u8> = hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    assert!(String::from_utf8_lossy(&output).contains("TRACE_MIDDLE"));
    assert_eq!(output.len(), 160_012);
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
    let wrong = || {
        completion(
            "REVIEWING: another commit (wrong)\nNo defects.",
            None,
            "stop",
        )
    };
    let mock = MockFetch::start(&fixture.root.join("run"), vec![models(), wrong(), wrong()]);
    let result = run(&fixture, &mock, "0.02");
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("exact commit"));
    // Asked once to restate, then refused.
    assert_eq!(mock.requests().len(), 3);
    fixture.no_workspaces();
}

/// A final review with a preamble before its identity line is asked for
/// again once, beginning with that line; the restated review is the one
/// written, and the trace counts the restatement.
#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn a_review_with_a_preamble_is_asked_to_restate_once() {
    let fixture = Fixture::new();
    let preamble = completion(
        &format!(
            "The tests pass.\n\nREVIEWING: fixture review ({})\nNo findings.",
            fixture.commit
        ),
        None,
        "stop",
    );
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![models(), preamble, fixture.final_reply()],
    );
    let result = run(&fixture, &mock, "0.02");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        stdout.starts_with("REVIEWING: fixture review ("),
        "{stdout}"
    );
    assert!(!stdout.contains("The tests pass."), "{stdout}");
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    // The ask takes the place of that step's status, a final request's,
    // and the review it answers carries no empty tool calls.
    let asked = requests[2].text();
    assert!(asked.contains("must begin with the exact line"), "{asked}");
    let after = asked.rsplit("\"role\":\"assistant\"").next().unwrap();
    assert!(!after.contains("Review harness status"), "{after}");
    assert!(!asked.contains("\"tool_calls\":[]"), "{asked}");
    let logs = fixture.logs();
    let metrics = td_agent::review_metrics::summarize(logs[0].as_bytes()).unwrap();
    assert_eq!(metrics.get("restatements").and_then(Json::as_u64), Some(1));
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
    let logs = fixture.logs();
    assert_eq!(logs.len(), 1);
    assert!(logs[0].contains("budget_reservation"));
    assert!(!logs[0].contains("request_body"));
    assert!(logs[0]
        .lines()
        .last()
        .unwrap()
        .contains("\"success\":false"));
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
    let interrupted = fixture.logs();
    assert_eq!(interrupted.len(), 1);
    assert!(interrupted[0].contains("request_body"));
    assert!(!interrupted[0].contains("\"kind\":\"end\""));
    mock.then(vec![models()]);
    let result = run(&fixture, &mock, "0");
    assert!(!result.status.success());
    fixture.no_workspaces();
}

#[test]
#[ignore = "uses the host CLI key fixture"]
fn diff_only_trace_keeps_rate_limit_and_malformed_response_bytes() {
    let fixture = Fixture::new();
    let limited = b"{\"error\":{\"message\":\"TRY AGAIN\"}}".to_vec();
    let malformed = b"malformed response \xff".to_vec();
    let headers = vec![("content-type".into(), "application/json".into())];
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![
            models(),
            Reply::Http {
                status: 429,
                headers: vec![
                    ("content-type".into(), "application/json".into()),
                    ("retry-after".into(), "0".into()),
                ],
                body: limited.clone(),
            },
            Reply::Http {
                status: 200,
                headers,
                body: malformed.clone(),
            },
        ],
    );
    let input = fixture.root.join("commit.txt");
    fs::write(&input, "commit 1234567890\n\n    fixture review\n").unwrap();
    let template = fixture.command(&mock, "0.02");
    let mut command = Command::new(PROGRAM);
    command
        .args([
            "review",
            "--model",
            MODEL,
            "--max-tokens",
            "4096",
            "--max-cost",
            "0.02",
        ])
        .arg(input)
        .stdin(Stdio::null());
    for (name, value) in template.get_envs() {
        if let Some(value) = value {
            command.env(name, value);
        }
    }
    let result = command.output().unwrap();
    assert!(!result.status.success());
    assert_eq!(mock.requests().len(), 3);
    let logs = fixture.logs();
    let events: Vec<Json> = logs[0]
        .lines()
        .map(|line| td_json::parse(line).unwrap())
        .collect();
    let hex: String = events
        .iter()
        .filter(|event| event.get("kind").and_then(Json::as_str) == Some("response_bytes_hex"))
        .map(|event| event.get("data").and_then(Json::as_str).unwrap())
        .collect();
    let raw: Vec<u8> = hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    assert_eq!(raw, [limited, malformed].concat());
    let summary = td_agent::review_metrics::summarize(logs[0].as_bytes()).unwrap();
    assert_eq!(summary.get("requests").and_then(Json::as_u64), Some(1));
    assert_eq!(
        summary.get("request_attempts").and_then(Json::as_u64),
        Some(2)
    );
    assert_eq!(
        summary.get("retry_attempts").and_then(Json::as_u64),
        Some(1)
    );
    assert_eq!(
        summary.get("unresolved_attempts").and_then(Json::as_u64),
        Some(0)
    );
    assert_eq!(
        summary.get("response_bytes").and_then(Json::as_u64),
        Some(raw.len() as u64)
    );
    assert_eq!(
        summary
            .get_path(&["http_status_counts", "429"])
            .and_then(Json::as_u64),
        Some(1)
    );
    assert_eq!(
        summary
            .get_path(&["http_status_counts", "200"])
            .and_then(Json::as_u64),
        Some(1)
    );
    assert_eq!(
        summary
            .get_path(&["finished_attempt_ms", "samples"])
            .and_then(Json::as_u64),
        Some(2)
    );
    assert!(logs[0].contains("retry_wait_ms"));
    assert!(logs[0].contains("request_error"));
    assert!(logs[0]
        .lines()
        .last()
        .unwrap()
        .contains("\"success\":false"));
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn connection_failures_close_the_logged_attempt_and_preserve_budget_and_cleanup() {
    let fixture = Fixture::new();
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![
            models(),
            Reply::Error("transport: fixture connection refused".into()),
        ],
    );
    let result = fixture.command(&mock, "0.02").output().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("fixture connection refused"));
    assert_eq!(mock.requests().len(), 2);
    let logs = fixture.logs();
    assert_eq!(logs.len(), 1);
    let summary = td_agent::review_metrics::summarize(logs[0].as_bytes()).unwrap();
    assert_eq!(summary.get("requests").and_then(Json::as_u64), Some(1));
    assert_eq!(
        summary.get("request_attempts").and_then(Json::as_u64),
        Some(1)
    );
    assert_eq!(
        summary.get("unresolved_attempts").and_then(Json::as_u64),
        Some(0)
    );
    assert_eq!(
        summary.get("request_error_count").and_then(Json::as_u64),
        Some(1)
    );
    assert_eq!(
        summary
            .get_path(&["finished_attempt_ms", "samples"])
            .and_then(Json::as_u64),
        Some(1)
    );
    let reservation = logs[0]
        .lines()
        .map(|line| td_json::parse(line).unwrap())
        .find(|event| event.get("kind").and_then(Json::as_str) == Some("budget_reservation"))
        .unwrap();
    let charged = reservation
        .get_path(&["data", "charged"])
        .and_then(Json::as_u64)
        .unwrap();
    let reserved = reservation
        .get_path(&["data", "reserved"])
        .and_then(Json::as_u64)
        .unwrap();
    assert!(reserved > 0);
    assert_eq!(
        summary.get("accounted_cost").and_then(Json::as_u64),
        Some(charged + reserved)
    );
    assert_eq!(
        summary.get("request_errors"),
        Some(&Json::Arr(vec![Json::from(
            "the request: fixture connection refused"
        )]))
    );
    assert_eq!(summary.get("response_header_ms"), Some(&Json::Null));
    assert_eq!(summary.get("http_status_counts"), Some(&Json::Obj(vec![])));
    assert_eq!(
        summary
            .get_path(&["end", "success"])
            .and_then(Json::as_bool),
        Some(false)
    );
    assert_eq!(
        summary
            .get_path(&["cleanup", "success"])
            .and_then(Json::as_bool),
        Some(true)
    );
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn binary_output_is_preserved_in_trace() {
    let fixture = Fixture::new();
    let bytes = 4 * 1024 * 1024;
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![
            models(),
            tool(
                "shell",
                Json::Obj(vec![(
                    "command".into(),
                    Json::Str(format!("head -c {bytes} /dev/zero; printf '\\377'")),
                )]),
            ),
            fixture.final_reply(),
        ],
    );
    let result = run(&fixture, &mock, "0.10");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let logs = fixture.logs();
    let hex: String = logs[0]
        .lines()
        .filter_map(|line| {
            let event = td_json::parse(line).unwrap();
            (event.get("kind").and_then(Json::as_str) == Some("tool_output_bytes_hex")).then(|| {
                event
                    .get("data")
                    .and_then(Json::as_str)
                    .unwrap()
                    .to_string()
            })
        })
        .collect();
    assert_eq!(hex.len(), (bytes + 1) * 2);
    assert!(hex.ends_with("ff"));
    assert!(hex
        .strip_suffix("ff")
        .unwrap()
        .bytes()
        .all(|byte| byte == b'0'));
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn capped_results_warn_on_repetition_and_recover_from_bad_arguments() {
    let fixture = Fixture::new();
    let args = Json::Obj(vec![(
        "command".into(),
        Json::Str("printf '%09000d' 0".into()),
    )]);
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![
            models(),
            tool("shell", args.clone()),
            tool("shell", args),
            completion("", Some(("read_file", "{bad json")), "tool_calls"),
            fixture.final_reply(),
        ],
    );
    let result = fixture
        .command(&mock, "0.10")
        .args([
            "--tool-output-bytes",
            "1024",
            "--tool-context-bytes",
            "3072",
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let requests = mock.requests();
    let body = td_json::parse(&requests.last().unwrap().text()).unwrap();
    let messages = body.get("messages").and_then(Json::as_arr).unwrap();
    let results: Vec<&str> = messages
        .iter()
        .filter(|m| m.get("role").and_then(Json::as_str) == Some("tool"))
        .map(|m| m.get("content").and_then(Json::as_str).unwrap())
        .collect();
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|text| text.len() <= 1024));
    assert!(results.iter().map(|text| text.len()).sum::<usize>() <= 3072);
    assert!(results[1].contains("identical arguments used 2 times"));
    assert!(results[2].contains("invalid tool arguments"));
    assert!(messages
        .last()
        .unwrap()
        .get("content")
        .and_then(Json::as_str)
        .unwrap()
        .contains("remaining"));
    let logs = fixture.logs();
    let metrics = td_agent::review_metrics::summarize(logs[0].as_bytes()).unwrap();
    assert_eq!(metrics.get("complete").and_then(Json::as_bool), Some(true));
    assert_eq!(
        metrics.get("repeated_calls").and_then(Json::as_u64),
        Some(1)
    );
    assert_eq!(
        metrics.get("shortened_results").and_then(Json::as_u64),
        Some(2)
    );
    assert_eq!(metrics.get("tool_calls").and_then(Json::as_u64), Some(3));
    fixture.no_workspaces();
}

fn amend_fixture(fixture: &mut Fixture) {
    for args in [
        vec!["add", "."],
        vec!["commit", "--amend", "--no-edit", "--quiet"],
    ] {
        let status = Command::new("git")
            .current_dir(&fixture.source)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Review fixture")
            .env("GIT_AUTHOR_EMAIL", "review@example.invalid")
            .env("GIT_COMMITTER_NAME", "Review fixture")
            .env("GIT_COMMITTER_EMAIL", "review@example.invalid")
            .status()
            .unwrap();
        assert!(status.success());
    }
    let out = Command::new("git")
        .current_dir(&fixture.source)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    fixture.commit = String::from_utf8(out.stdout).unwrap().trim().into();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn explicit_runner_overrides_an_absent_repository_runner() {
    let mut fixture = Fixture::new();
    fs::create_dir(fixture.source.join(".cargo")).unwrap();
    fs::write(fixture.source.join(".cargo/config.toml"), "[target.x86_64-unknown-linux-gnu]\nrunner = [\"/absent/repository-runner\", \"run-capped\"]\n").unwrap();
    amend_fixture(&mut fixture);
    let mock = MockFetch::start(&fixture.root.join("run"), vec![models(), tool("shell", Json::Obj(vec![
        ("command".into(), Json::Str("mkdir -p \"$CARGO_TARGET_DIR/project\"; cp Cargo.toml Cargo.lock fixture.rs \"$CARGO_TARGET_DIR/project/\"; cargo test --offline --locked --manifest-path \"$CARGO_TARGET_DIR/project/Cargo.toml\"".into())),
    ])), fixture.final_reply()]);
    let runner = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target/release/td-builder");
    let result = fixture
        .command(&mock, "0.10")
        .arg("--test-runner")
        .arg(&runner)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let logs = fixture.logs();
    assert!(logs[0].contains("test_runner") && logs[0].contains("sha256"));
    assert!(logs[0].contains("1 passed; 0 failed"), "{}", logs[0]);
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn anthropic_requests_cache_a_stable_conversation_prefix() {
    let fixture = Fixture::new();
    let model = "anthropic/claude-haiku-4.5";
    let replace = |reply: Reply| match reply {
        Reply::Http {
            status,
            headers,
            body,
        } => Reply::Http {
            status,
            headers,
            body: String::from_utf8(body)
                .unwrap()
                .replace(MODEL, model)
                .into_bytes(),
        },
        _ => panic!("expected HTTP fixture"),
    };
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![
            models(),
            replace(tool(
                "shell",
                Json::Obj(vec![(
                    "command".into(),
                    Json::Str("printf CACHE_FIXTURE".into()),
                )]),
            )),
            replace(fixture.final_reply()),
        ],
    );
    let cmd = fixture.command(&mock, "1.00");
    let args: Vec<_> = cmd
        .get_args()
        .map(|s| {
            if s == MODEL {
                std::ffi::OsString::from(model)
            } else {
                s.to_os_string()
            }
        })
        .collect();
    let mut selected = Command::new(PROGRAM);
    selected.args(args);
    for (name, value) in cmd.get_envs() {
        if let Some(value) = value {
            selected.env(name, value);
        }
    }
    let result = selected.stdin(Stdio::null()).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let requests = mock.requests();
    let first = td_json::parse(&requests[1].text()).unwrap();
    let next = td_json::parse(&requests[2].text()).unwrap();
    assert!(first.get("cache_control").is_some());
    assert_eq!(first.get("cache_control"), next.get("cache_control"));
    assert_eq!(first.get("tools"), next.get("tools"));
    let prefix = first.get("messages").and_then(Json::as_arr).unwrap();
    let history = next.get("messages").and_then(Json::as_arr).unwrap();
    assert_eq!(history.get(..prefix.len()).unwrap(), prefix);
    assert!(prefix
        .last()
        .unwrap()
        .get("content")
        .and_then(Json::as_str)
        .unwrap()
        .contains("conservative reservation"));
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn explicit_vendor_resolves_locked_dependencies_offline() {
    let mut fixture = Fixture::new();
    let vendor = fixture.root.join("vendor");
    let package = vendor.join("review-vendor-fixture-0.1.0");
    fs::create_dir_all(&package).unwrap();
    let manifest = "[package]\nname = \"review-vendor-fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[lib]\npath = \"lib.rs\"\n";
    let source = "pub fn value() -> u32 { 4 }\n";
    fs::write(package.join("Cargo.toml"), manifest).unwrap();
    fs::write(package.join("lib.rs"), source).unwrap();
    let checksum = "0".repeat(64);
    let hash = |path: &Path| {
        let output = Command::new("sha256sum").arg(path).output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    };
    let hash_manifest = hash(&package.join("Cargo.toml"));
    let hash_source = hash(&package.join("lib.rs"));
    fs::write(package.join(".cargo-checksum.json"), format!("{{\"package\":\"{checksum}\",\"files\":{{\"Cargo.toml\":\"{hash_manifest}\",\"lib.rs\":\"{hash_source}\"}}}}")).unwrap();
    let mut root_manifest = fs::read_to_string(fixture.source.join("Cargo.toml")).unwrap();
    root_manifest.push_str("[dependencies]\nreview-vendor-fixture = \"=0.1.0\"\n");
    fs::write(fixture.source.join("Cargo.toml"), root_manifest).unwrap();
    fs::write(fixture.source.join("Cargo.lock"), format!("version = 4\n[[package]]\nname = \"review-process-fixture\"\nversion = \"0.1.0\"\ndependencies = [\"review-vendor-fixture\"]\n[[package]]\nname = \"review-vendor-fixture\"\nversion = \"0.1.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"{checksum}\"\n")).unwrap();
    amend_fixture(&mut fixture);
    let mock = MockFetch::start(&fixture.root.join("run"), vec![models(), tool("shell", Json::Obj(vec![
        ("command".into(), Json::Str("mkdir -p \"$CARGO_TARGET_DIR/project\"; cp Cargo.toml Cargo.lock fixture.rs \"$CARGO_TARGET_DIR/project/\"; cargo test --offline --locked --manifest-path \"$CARGO_TARGET_DIR/project/Cargo.toml\"".into())),
    ])), fixture.final_reply()]);
    let result = fixture
        .command(&mock, "0.10")
        .arg("--vendor")
        .arg(&vendor)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let logs = fixture.logs();
    assert!(logs[0].contains("1 passed; 0 failed"), "{}", logs[0]);
    let preflight = logs[0]
        .lines()
        .map(|l| td_json::parse(l).unwrap())
        .find(|e| e.get("kind").and_then(Json::as_str) == Some("test_preflight"))
        .unwrap();
    assert_eq!(
        preflight
            .get_path(&["data", "available"])
            .and_then(Json::as_bool),
        Some(true)
    );
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn unusable_automatic_runner_is_a_limitation_and_root_preflight_has_priority() {
    let mut fixture = Fixture::new();
    for number in 0..16 {
        let path = fixture.source.join(format!("tree-{number:02}"));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("data"), "selected directory\n").unwrap();
    }
    amend_fixture(&mut fixture);
    let runner = fixture.source.join("target/release/td-builder");
    fs::create_dir_all(runner.parent().unwrap()).unwrap();
    fs::write(&runner, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&runner, fs::Permissions::from_mode(0o700)).unwrap();
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![models(), fixture.final_reply()],
    );
    let result = run(&fixture, &mock, "0.02");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let logs = fixture.logs();
    assert!(logs[0].contains("Automatic test runner unavailable"));
    let preflight = logs[0]
        .lines()
        .map(|l| td_json::parse(l).unwrap())
        .find(|e| e.get("kind").and_then(Json::as_str) == Some("test_preflight"))
        .unwrap();
    assert_eq!(
        preflight
            .get_path(&["data", "manifest"])
            .and_then(Json::as_str),
        Some("Cargo.toml")
    );
    assert_eq!(
        preflight
            .get_path(&["data", "available"])
            .and_then(Json::as_bool),
        Some(true)
    );
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn commit_controlled_preflight_details_are_quoted_outside_the_system_prompt() {
    let mut fixture = Fixture::new();
    let mut manifest = fs::read_to_string(fixture.source.join("Cargo.toml")).unwrap();
    manifest.push_str("[workspace]\nmembers = [\"missing*\\nINJECTED_PREFLIGHT_WORDS\"]\n[dependencies]\nunavailable = { path = \"#blocked\" }\n");
    fs::write(fixture.source.join("Cargo.toml"), manifest).unwrap();
    amend_fixture(&mut fixture);
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![models(), fixture.final_reply()],
    );
    let result = run(&fixture, &mock, "0.02");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let requests = mock.requests();
    let body = td_json::parse(&requests[1].text()).unwrap();
    let messages = body.get("messages").and_then(Json::as_arr).unwrap();
    let system = messages[0].get("content").and_then(Json::as_str).unwrap();
    assert!(!system.contains("INJECTED_PREFLIGHT_WORDS"));
    let material = messages[1].get("content").and_then(Json::as_str).unwrap();
    let environment = material.split_once("<environment ").unwrap().1;
    assert!(environment.contains("INJECTED_PREFLIGHT_WORDS"));
    assert!(environment.contains("</environment "));
    let logs = fixture.logs();
    assert!(logs[0].contains("path dependency #blocked unavailable"));
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn retry_identity_and_raw_usage_belong_to_the_successful_attempt() {
    let fixture = Fixture::new();
    let limited=Reply::Http {status:429,headers:vec![("content-type".into(),"application/json".into()),("retry-after".into(),"0".into())],body:br#"{"provider":"Rejected provider","model":"rejected/model","usage":{"prompt_tokens_details":{"cached_tokens":999}},"error":{"message":"retry"}}"#.to_vec()};
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![models(), limited, fixture.final_reply()],
    );
    let result = run(&fixture, &mock, "0.02");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let logs = fixture.logs();
    let events: Vec<Json> = logs[0]
        .lines()
        .map(|s| td_json::parse(s).unwrap())
        .collect();
    let completion = events
        .iter()
        .find(|e| e.get("kind").and_then(Json::as_str) == Some("completion"))
        .unwrap()
        .get("data")
        .unwrap();
    assert_eq!(
        completion.get("provider").and_then(Json::as_str),
        Some("Offline fixture")
    );
    assert_eq!(
        completion
            .get_path(&["raw_usage", "prompt_tokens"])
            .and_then(Json::as_u64),
        Some(100)
    );
    let summary = td_agent::review_metrics::summarize(logs[0].as_bytes()).unwrap();
    assert_eq!(summary.get_path(&["tokens", "cached"]), Some(&Json::Null));
    let rows = summary
        .get_path(&["diagnostics", "requests"])
        .unwrap()
        .as_arr()
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get("served_provider").and_then(Json::as_str),
        Some("Offline fixture")
    );
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn review_compacts_without_losing_recent_tool_pairs_or_full_trace() {
    let fixture = Fixture::new();
    let args = Json::Obj(vec![(
        "command".into(),
        Json::Str("printf '%09000d' 0".into()),
    )]);
    let mut latest = tool("shell", args.clone());
    if let Reply::Http { body, .. } = &mut latest {
        let mut value = td_json::parse_slice(body).unwrap();
        if let Some(Json::Obj(fields)) = value.get_mut("usage") {
            if let Some((_, slot)) = fields.iter_mut().find(|(k, _)| k == "prompt_tokens") {
                *slot = Json::from(6000u64);
            }
        }
        let choices = match value.get_mut("choices").unwrap() {
            Json::Arr(v) => v,
            _ => panic!("choices"),
        };
        let message = choices.first_mut().unwrap().get_mut("message").unwrap();
        if let Json::Obj(fields) = message {
            fields.push((
                "reasoning_details".into(),
                td_json::parse(r#"[{"type":"reasoning.encrypted","data":"opaque-recent"}]"#)
                    .unwrap(),
            ));
        }
        *body = value.to_string().into_bytes();
    }
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![
            models(),
            tool("shell", args),
            latest,
            completion(
                "Inspected two outputs; no test was run. Continue reviewing the exact commit.",
                None,
                "stop",
            ),
            fixture.final_reply(),
        ],
    );
    let result = fixture
        .command(&mock, "0.02")
        .args(["--context-tokens", "14000"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    let summary = td_json::parse(&requests[3].text()).unwrap();
    assert!(summary.get("max_tokens").unwrap().as_u64().unwrap() <= 1400);
    assert!(requests[3]
        .text()
        .contains("write a concise handoff summary"));
    let final_body = td_json::parse(&requests[4].text()).unwrap();
    let messages = final_body.get("messages").unwrap().as_arr().unwrap();
    let recent = messages
        .iter()
        .find(|m| m.get("role").and_then(Json::as_str) == Some("assistant"))
        .unwrap();
    assert_eq!(
        recent.get_path(&["reasoning_details"]).unwrap().to_string(),
        r#"[{"type":"reasoning.encrypted","data":"opaque-recent"}]"#
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.get("role").and_then(Json::as_str) == Some("assistant"))
            .count(),
        1
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.get("role").and_then(Json::as_str) == Some("tool"))
            .count(),
        1
    );
    assert!(requests[4].text().contains(&fixture.commit));
    assert!(!requests[4]
        .text()
        .contains("write a concise handoff summary"));
    let logs = fixture.logs();
    assert_eq!(
        logs[0]
            .lines()
            .filter(|s| s.contains("\"kind\":\"tool_full_result\""))
            .count(),
        2
    );
    let metrics = td_agent::review_metrics::summarize(logs[0].as_bytes()).unwrap();
    assert_eq!(
        metrics.get("context_compactions").and_then(Json::as_u64),
        Some(1)
    );
    assert_eq!(metrics.get("requests").and_then(Json::as_u64), Some(4));
    assert!(
        metrics
            .get("compacted_context_bytes")
            .unwrap()
            .as_u64()
            .unwrap()
            > 0
    );
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn oversized_compaction_input_is_bounded_before_the_summary_request() {
    let fixture = Fixture::new();
    let args = Json::Obj(vec![(
        "command".into(),
        Json::Str("printf '%09000d' 0".into()),
    )]);
    let mut latest = tool(
        "shell",
        Json::Obj(vec![(
            "command".into(),
            Json::Str("printf '%01000d' 0".into()),
        )]),
    );
    if let Reply::Http { body, .. } = &mut latest {
        let mut value = td_json::parse_slice(body).unwrap();
        if let Some(Json::Obj(fields)) = value.get_mut("usage") {
            if let Some((_, slot)) = fields.iter_mut().find(|(k, _)| k == "prompt_tokens") {
                *slot = Json::from(12000u64);
            }
        }
        let choices = match value.get_mut("choices").unwrap() {
            Json::Arr(v) => v,
            _ => panic!("choices"),
        };
        let message = choices.first_mut().unwrap().get_mut("message").unwrap();
        if let Json::Obj(fields) = message {
            fields.push((
                "reasoning_details".into(),
                td_json::parse(r#"[{"type":"reasoning.encrypted","data":"opaque-recent"}]"#)
                    .unwrap(),
            ));
        }
        *body = value.to_string().into_bytes();
    }
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![
            models(),
            tool("shell", args),
            latest,
            completion(
                "Inspected two outputs; no test was run. Continue reviewing the exact commit.",
                None,
                "stop",
            ),
            fixture.final_reply(),
        ],
    );
    let result = fixture
        .command(&mock, "0.02")
        .args(["--context-tokens", "14000"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    let summary = td_json::parse(&requests[3].text()).unwrap();
    assert!(summary.get("max_tokens").unwrap().as_u64().unwrap() <= 1400);
    assert!(requests[3]
        .text()
        .contains("write a concise handoff summary"));
    let final_body = td_json::parse(&requests[4].text()).unwrap();
    let messages = final_body.get("messages").unwrap().as_arr().unwrap();
    let recent = messages
        .iter()
        .find(|m| m.get("role").and_then(Json::as_str) == Some("assistant"))
        .unwrap();
    assert_eq!(
        recent.get_path(&["reasoning_details"]).unwrap().to_string(),
        r#"[{"type":"reasoning.encrypted","data":"opaque-recent"}]"#
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.get("role").and_then(Json::as_str) == Some("assistant"))
            .count(),
        1
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.get("role").and_then(Json::as_str) == Some("tool"))
            .count(),
        1
    );
    assert!(requests[4].text().contains(&fixture.commit));
    assert!(!requests[4]
        .text()
        .contains("write a concise handoff summary"));
    let logs = fixture.logs();
    assert!(logs[0].contains("context_compaction_input"));
    assert_eq!(
        logs[0]
            .lines()
            .filter(|s| s.contains("\"kind\":\"tool_full_result\""))
            .count(),
        2
    );
    let metrics = td_agent::review_metrics::summarize(logs[0].as_bytes()).unwrap();
    assert_eq!(
        metrics.get("context_compactions").and_then(Json::as_u64),
        Some(1)
    );
    assert_eq!(metrics.get("requests").and_then(Json::as_u64), Some(4));
    assert_eq!(
        metrics
            .get("shortened_summary_inputs")
            .and_then(Json::as_u64),
        Some(1)
    );
    assert!(
        metrics
            .get("compacted_context_bytes")
            .unwrap()
            .as_u64()
            .unwrap()
            > 0
    );
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn provider_context_refusal_compacts_once_and_preserves_its_reservation() {
    let fixture = Fixture::new();
    let args = Json::Obj(vec![("command".into(), Json::Str("printf STEP".into()))]);
    let refusal = Reply::Http {
        status: 400,
        headers: vec![("content-type".into(), "application/json".into())],
        body: br#"{"error":{"message":"context length exceeded"}}"#.to_vec(),
    };
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![
            models(),
            tool("shell", args.clone()),
            tool("shell", args),
            refusal,
            completion(
                "Two commands produced STEP. No tests were run.",
                None,
                "stop",
            ),
            fixture.final_reply(),
        ],
    );
    let result = run(&fixture, &mock, "0.10");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(mock.requests().len(), 6);
    let logs = fixture.logs();
    assert!(logs[0].contains("context_refusal"));
    let metrics = td_agent::review_metrics::summarize(logs[0].as_bytes()).unwrap();
    assert_eq!(
        metrics.get("context_compactions").and_then(Json::as_u64),
        Some(1)
    );
    assert!(
        metrics
            .get("unreported_accounting")
            .unwrap()
            .as_u64()
            .unwrap()
            > 0
    );
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn repeated_provider_context_refusal_is_not_retried_indefinitely() {
    let fixture = Fixture::new();
    let args = Json::Obj(vec![("command".into(), Json::Str("printf STEP".into()))]);
    let refusal = || Reply::Http {
        status: 413,
        headers: vec![("content-type".into(), "application/json".into())],
        body: br#"{"error":{"message":"context length exceeded"}}"#.to_vec(),
    };
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![
            models(),
            tool("shell", args.clone()),
            tool("shell", args),
            refusal(),
            completion(
                "Two commands produced STEP. No tests were run.",
                None,
                "stop",
            ),
            refusal(),
        ],
    );
    let result = run(&fixture, &mock, "0.10");
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert_eq!(mock.requests().len(), 6);
    let logs = fixture.logs();
    let metrics = td_agent::review_metrics::summarize(logs[0].as_bytes()).unwrap();
    assert_eq!(
        metrics
            .get("context_compaction_attempts")
            .and_then(Json::as_u64),
        Some(1)
    );
    assert_eq!(
        metrics.get("context_refusals").and_then(Json::as_u64),
        Some(1)
    );
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn compaction_leaves_the_last_affordable_request_for_the_final_answer() {
    let fixture = Fixture::new();
    let args = Json::Obj(vec![(
        "command".into(),
        Json::Str("printf '%09000d' 0".into()),
    )]);
    let mut latest = tool("shell", args.clone());
    if let Reply::Http { body, .. } = &mut latest {
        let mut value = td_json::parse_slice(body).unwrap();
        if let Some(Json::Obj(fields)) = value.get_mut("usage") {
            if let Some((_, slot)) = fields.iter_mut().find(|(k, _)| k == "prompt_tokens") {
                *slot = Json::from(6000u64);
            }
        }
        *body = value.to_string().into_bytes();
    }
    let mock = MockFetch::start(
        &fixture.root.join("run"),
        vec![models(), tool("shell", args), latest, fixture.final_reply()],
    );
    let result = fixture
        .command(&mock, "0.0045")
        .args(["--context-tokens", "14000"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests[3].text().contains("produce the final review now"));
    assert!(!requests[3]
        .text()
        .contains("write a concise handoff summary"));
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn unusable_context_room_fails_before_a_paid_request() {
    let fixture = Fixture::new();
    let mock = MockFetch::start(&fixture.root.join("run"), vec![models()]);
    let result = fixture
        .command(&mock, "0.02")
        .args(["--context-tokens", "1024"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("minimum useful completion tokens"));
    assert_eq!(mock.requests().len(), 1);
    fixture.no_workspaces();
}

#[test]
#[ignore = "needs user namespaces, TD_AGENT_JAIL and TD_AGENT_TXT"]
fn request_limit_reserves_its_last_slot_for_a_final_review() {
    let fixture = Fixture::new();
    let args = Json::Obj(vec![("command".into(), Json::Str("printf STEP".into()))]);
    let mut replies = vec![models()];
    for _ in 0..63 {
        replies.push(tool("shell", args.clone()));
    }
    replies.push(fixture.final_reply());
    let mock = MockFetch::start(&fixture.root.join("run"), replies);
    let result = run(&fixture, &mock, "0.10");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 65);
    assert!(requests
        .last()
        .unwrap()
        .text()
        .contains("produce the final review now"));
    assert!(!requests
        .last()
        .unwrap()
        .text()
        .contains("write a concise handoff summary"));
    fixture.no_workspaces();
}
