//! Inspect the resolved public surface of the pinned compiler's diagnostic graph.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use td_engine::json::Json;
type Result<T> = std::result::Result<T, String>;

fn field<'a>(value: &'a Json, name: &str) -> Result<&'a Json> {
    value
        .get(name)
        .ok_or_else(|| format!("API schema lacks {name}"))
}
fn object(value: &Json) -> Result<&[(String, Json)]> {
    match value {
        Json::Obj(values) => Ok(values),
        _ => Err("API schema expected an object".into()),
    }
}
fn array(value: &Json) -> Result<&[Json]> {
    value
        .as_arr()
        .ok_or_else(|| "API schema expected an array".into())
}
fn number(value: &Json) -> Result<&str> {
    match value {
        Json::Num(n) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => Ok(n),
        _ => Err("API schema expected an unsigned ID".into()),
    }
}
fn text(value: &Json) -> Result<&str> {
    value
        .as_str()
        .ok_or_else(|| "API schema expected text".into())
}
fn table(value: &Json) -> Result<BTreeMap<&str, &Json>> {
    let entries = object(value)?;
    if entries.len() > 65536 {
        return Err("API table exceeds 65536 entries".into());
    }
    let mut map = BTreeMap::new();
    for (key, value) in entries {
        if map.insert(key.as_str(), value).is_some() {
            return Err("API table has duplicate IDs".into());
        }
    }
    Ok(map)
}
struct Surface<'a> {
    index: BTreeMap<&'a str, &'a Json>,
    paths: BTreeMap<&'a str, &'a Json>,
    crates: BTreeMap<&'a str, &'a Json>,
    visited: BTreeSet<&'a str>,
    pending: Vec<&'a str>,
    local: &'a str,
}
impl<'a> Surface<'a> {
    fn add(&mut self, id: &'a Json) -> Result<()> {
        let id = number(id)?;
        if self.visited.insert(id) {
            if self.visited.len() > 65536 {
                return Err("API graph exceeds 65536 reachable items".into());
            }
            self.pending.push(id);
        }
        Ok(())
    }
    fn refs(&mut self, value: &'a Json, depth: usize) -> Result<()> {
        if depth > 128 {
            return Err("API reference nesting exceeds 128".into());
        }
        match value {
            Json::Obj(values) => {
                for (key, value) in values {
                    if key == "id" {
                        if *value != Json::Null {
                            self.add(value)?;
                        }
                    } else {
                        self.refs(value, depth + 1)?;
                    }
                }
            }
            Json::Arr(values) => {
                for value in values {
                    self.refs(value, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn children(&mut self, children: &'a Json, all: bool) -> Result<()> {
        for id in array(children)? {
            if *id == Json::Null {
                continue;
            }
            let item = self
                .index
                .get(number(id)?)
                .copied()
                .ok_or("API child is absent from index")?;
            if all || field(item, "visibility")?.as_str() == Some("public") {
                self.add(id)?;
            }
        }
        Ok(())
    }
    fn fields(&mut self, shape: &'a Json, all: bool) -> Result<()> {
        if shape.as_str() == Some("unit") || shape.as_str() == Some("plain") {
            return Ok(());
        }
        let [(kind, fields)] = object(shape)? else {
            return Err("API field shape is ambiguous".into());
        };
        match kind.as_str() {
            "tuple" => self.children(fields, all),
            "plain" | "struct" => self.children(field(fields, "fields")?, all),
            _ => Err(format!("unsupported API field shape {kind}")),
        }
    }
    fn visit(&mut self, id: &'a str) -> Result<()> {
        let item = self.index.get(id).copied();
        let definition = item
            .or_else(|| self.paths.get(id).copied())
            .ok_or_else(|| {
                format!("unresolved public API ID {id}; define facade handles at module scope")
            })?;
        let crate_id = number(field(definition, "crate_id")?)?;
        if crate_id != self.local {
            let external = self
                .crates
                .get(crate_id)
                .copied()
                .ok_or("API external crate is unresolved")?;
            let name = text(field(external, "name")?)?;
            return if matches!(name, "std" | "core" | "alloc") {
                Ok(())
            } else {
                Err(format!(
                    "public crypto API exposes external crate {name} (ID {id})"
                ))
            };
        }
        let item = item.ok_or("API local definition is absent from index")?;
        let [(kind, body)] = object(field(item, "inner")?)? else {
            return Err("API item kind is ambiguous".into());
        };
        match kind.as_str() {
            "module" => self.children(field(body, "items")?, false),
            "struct" | "union" => {
                self.refs(field(body, "generics")?, 0)?;
                self.children(field(body, "impls")?, true)?;
                if kind == "union" {
                    self.children(field(body, "fields")?, false)
                } else {
                    self.fields(field(body, "kind")?, false)
                }
            }
            "enum" => {
                self.refs(field(body, "generics")?, 0)?;
                self.children(field(body, "variants")?, true)?;
                self.children(field(body, "impls")?, true)
            }
            "variant" => self.fields(field(body, "kind")?, true),
            "trait" => {
                self.refs(field(body, "generics")?, 0)?;
                self.refs(field(body, "bounds")?, 0)?;
                self.children(field(body, "items")?, true)?;
                self.children(field(body, "implementations")?, true)
            }
            "impl" => {
                for (key, value) in object(body)? {
                    if key != "items" {
                        self.refs(value, 0)?;
                    }
                }
                self.children(field(body, "items")?, *field(body, "trait")? != Json::Null)
            }
            "use" => self.add(field(body, "id")?),
            "function" | "assoc_type" | "assoc_const" | "constant" | "static" | "type_alias"
            | "struct_field" => self.refs(body, 0),
            _ => Err(format!("unsupported public API item {kind}")),
        }
    }
}
pub(crate) fn inspect(document: &Json) -> Result<usize> {
    if number(field(document, "format_version")?)? != "57"
        || *field(document, "includes_private")? != Json::Bool(true)
    {
        return Err("API diagnostic requires Rust 1.96 schema 57 with private items".into());
    }
    let target = field(document, "target")?;
    if text(field(target, "triple")?)? != "x86_64-unknown-linux-musl" {
        return Err("API diagnostic target differs from the portable target".into());
    }
    let mut enabled = BTreeSet::new();
    for feature in array(field(target, "target_features")?)? {
        match field(feature, "globally_enabled")? {
            Json::Bool(true) => {
                enabled.insert(text(field(feature, "name")?)?);
            }
            Json::Bool(false) => {}
            _ => return Err("API target feature lacks a boolean state".into()),
        }
    }
    if enabled != BTreeSet::from(["fxsr", "sse", "sse2", "x87"]) {
        return Err("API diagnostic CPU features differ from baseline x86-64".into());
    }
    let index = table(field(document, "index")?)?;
    let root = field(document, "root")?;
    let local = number(field(
        *index.get(number(root)?).ok_or("API root is absent")?,
        "crate_id",
    )?)?;
    let mut surface = Surface {
        index,
        paths: table(field(document, "paths")?)?,
        crates: table(field(document, "external_crates")?)?,
        visited: BTreeSet::new(),
        pending: Vec::new(),
        local,
    };
    surface.add(root)?;
    while let Some(id) = surface.pending.pop() {
        surface.visit(id)?;
    }
    Ok(surface.visited.len())
}

fn parse(input: &str) -> Result<Json> {
    if input.len() > 16 * 1024 * 1024 {
        return Err("API diagnostic exceeds 16 MiB".into());
    }
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for byte in input.bytes() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'{' | b'[' => {
                    depth += 1;
                    if depth > 128 {
                        return Err("API JSON nesting exceeds 128".into());
                    }
                }
                b'}' | b']' => depth = depth.checked_sub(1).ok_or("unbalanced API JSON")?,
                _ => {}
            }
        }
    }
    td_engine::json::parse(input).map_err(|e| format!("API JSON: {e}"))
}

fn compiler(program: &str, name: &str) -> Command {
    let mut command = Command::new(format!("/rust/bin/{program}"));
    command
        .current_dir("/tmp")
        .env_clear()
        .env("LD_LIBRARY_PATH", "/lib64:/gcc-runtime:/zlib:/rust/lib")
        .args([
            "--edition",
            "2021",
            "--target",
            "x86_64-unknown-linux-musl",
            "--crate-name",
            name,
            "-Copt-level=3",
            "-Cdebug-assertions=off",
            "-Ldependency=/output/api",
        ])
        .args(crate::crypto_isolated::rust_flags().split('\x1f'))
        .stdin(Stdio::null());
    crate::host_bin::arm_check_child(&mut command);
    command
}

// Rustdoc adds doc and retains debug_assertions despite release codegen flags.
const CFG_GUARD: &[&str] = &[
    "--check-cfg=cfg(doc,debug_assertions,values())",
    "-Funexpected_cfgs",
];
const PRIVATE_DEPENDENCIES: &[&str] = &["rustls", "aws_lc_rs", "webpki_roots"];

fn diagnostic(name: &str, source: &Path, destination: &Path, library: &Path) -> Command {
    let mut command = compiler("rustdoc", name);
    command
        .env("RUSTC_BOOTSTRAP", name)
        .args(CFG_GUARD)
        .args([
            "--document-private-items",
            "--document-hidden-items",
            "-Zunstable-options",
            "--output-format",
            "json",
            "--extern",
        ])
        .arg(format!("fake_backend={}", library.display()))
        .arg("-o")
        .arg(destination)
        .arg(source);
    command
}

fn inspect_file(path: &Path) -> Result<usize> {
    let output = crate::crypto_isolated::read_output(path, "API JSON", 16 * 1024 * 1024)?;
    inspect(&parse(&output)?)
}

fn cargo_diagnostic(
    verb: &str,
    manifest: &Path,
    package: &str,
    structured_messages: bool,
) -> Command {
    let mut command = crate::crypto_isolated::cargo_manifest(verb, manifest);
    command
        .env("CARGO_TARGET_DIR", "/output/api-target")
        .args([
            "--release",
            "--package",
            package,
            "--lib",
            "--message-format",
            if structured_messages {
                "json"
            } else {
                "json-render-diagnostics"
            },
            "--",
        ])
        .args(CFG_GUARD);
    if verb == "rustdoc" {
        command
            .env("RUSTC_BOOTSTRAP", package.replace('-', "_"))
            .env(
                "CARGO_ENCODED_RUSTDOCFLAGS",
                crate::crypto_isolated::rust_flags(),
            )
            .args([
                "-Copt-level=3",
                "-Cdebug-assertions=off",
                "--document-private-items",
                "--document-hidden-items",
                "-Zunstable-options",
                "--output-format",
                "json",
            ]);
    }
    command
}

fn private_dependencies(command: &mut Command, package: &str, dependencies: &[&str]) {
    command
        .env("RUSTC_BOOTSTRAP", package.replace('-', "_"))
        .args(["-Zunstable-options", "-Fexported_private_dependencies"]);
    for dependency in dependencies {
        command.arg("--extern").arg(format!("priv:{dependency}"));
    }
}

/// Diagnose a second compilation; unstable options never produce shipping bytes.
pub(crate) fn qualify(receipt: &mut String) -> Result<()> {
    let root = Path::new("/output/api");
    fs::create_dir(root).map_err(|e| format!("API scratch: {e}"))?;
    let manifest = Path::new("/source/td-mta/Cargo.toml");
    let mut command = cargo_diagnostic("rustc", manifest, "td-crypto", false);
    crate::crypto_isolated::command_record(&command, "api-compile", receipt)?;
    crate::crypto_isolated::bounded_output(&mut command, "api-compile", 8 * 1024 * 1024, 1200)?;
    let mut command = cargo_diagnostic("rustc", manifest, "td-crypto", false);
    private_dependencies(&mut command, "td-crypto", PRIVATE_DEPENDENCIES);
    crate::crypto_isolated::command_record(&command, "api-private-dependencies", receipt)?;
    crate::crypto_isolated::bounded_output(
        &mut command,
        "api-private-dependencies",
        8 * 1024 * 1024,
        1200,
    )?;
    let mut command = cargo_diagnostic("rustdoc", manifest, "td-crypto", false);
    crate::crypto_isolated::command_record(&command, "api-rustdoc", receipt)?;
    crate::crypto_isolated::bounded_output(&mut command, "api-rustdoc", 8 * 1024 * 1024, 1200)?;
    let count = inspect_file(Path::new(
        "/output/api-target/x86_64-unknown-linux-musl/doc/td_crypto.json",
    ))?;
    fixtures(root)?;
    cargo_fixtures(root, receipt)?;
    receipt.push_str(&format!(
        "api-schema 57\napi-target x86_64-unknown-linux-musl\napi-cpu-features fxsr,sse,sse2,x87\napi-reachable-items {count}\napi-fixtures {}\n",
        FIXTURES.len()
    ));
    Ok(())
}

fn cargo_fixtures(root: &Path, receipt: &mut String) -> Result<()> {
    let manifest = root.join("Cargo.toml");
    let source = root.join("lib.rs");
    fs::write(&manifest, "[package]\nname=\"api_probe\"\nversion=\"0.0.0\"\nedition=\"2021\"\n[lib]\npath=\"lib.rs\"\n[dependencies]\napi_probe_backend={path=\"backend\"}\n[workspace]\n").map_err(|e| e.to_string())?;
    let backend = root.join("backend");
    fs::create_dir(&backend).map_err(|e| e.to_string())?;
    fs::write(backend.join("Cargo.toml"), "[package]\nname=\"api_probe_backend\"\nversion=\"0.0.0\"\nedition=\"2021\"\n[lib]\npath=\"lib.rs\"\n").map_err(|e| e.to_string())?;
    fs::write(backend.join("lib.rs"), "pub struct Key;").map_err(|e| e.to_string())?;
    fs::write(
        root.join("Cargo.lock"),
        "version=4\n[[package]]\nname=\"api_probe\"\nversion=\"0.0.0\"\ndependencies=[\"api_probe_backend\"]\n[[package]]\nname=\"api_probe_backend\"\nversion=\"0.0.0\"\n",
    )
    .map_err(|e| e.to_string())?;
    fs::write(&source, "#[cfg(all(target_env=\"musl\",target_arch=\"x86_64\",target_feature=\"crt-static\",not(test)))] pub struct Qualified;").map_err(|e| e.to_string())?;
    let mut command = cargo_diagnostic("rustdoc", &manifest, "api_probe", true);
    crate::crypto_isolated::command_record(&command, "api-cargo-target", receipt)?;
    crate::crypto_isolated::bounded_output(&mut command, "api-cargo-target", 8 * 1024 * 1024, 120)?;
    let document = parse(&crate::crypto_isolated::read_output(
        Path::new("/output/api-target/x86_64-unknown-linux-musl/doc/api_probe.json"),
        "Cargo API fixture",
        16 * 1024 * 1024,
    )?)?;
    inspect(&document)?;
    if !object(field(&document, "index")?)?.iter().any(|(_, item)| {
        item.get("name").and_then(Json::as_str) == Some("Qualified")
            && item.get("visibility").and_then(Json::as_str) == Some("public")
    }) {
        return Err("Cargo rustdoc lost the static musl production cfg".into());
    }
    fs::write(&source, "#[cfg(not(doc))] pub struct Hidden;").map_err(|e| e.to_string())?;
    let mut command = cargo_diagnostic("rustc", &manifest, "api_probe", true);
    crate::crypto_isolated::command_record(&command, "api-cargo-guard", receipt)?;
    let output = crate::crypto_isolated::bounded_exit_output(
        &mut command,
        "api-cargo-guard",
        8 * 1024 * 1024,
        120,
        101,
    )?;
    if !output.lines().any(|line| {
        crate::crypto_isolated::artifact_json(line)
            .ok()
            .is_some_and(|value| value.get("message").is_some_and(unexpected_cfg_error))
    }) {
        return Err("Cargo API guard did not produce the expected compiler error".into());
    }
    fs::write(&source, "pub trait Local {} impl dyn Local {pub fn leak(&self)->api_probe_backend::Key {api_probe_backend::Key}}").map_err(|e| e.to_string())?;
    let mut command = cargo_diagnostic("rustc", &manifest, "api_probe", true);
    crate::crypto_isolated::command_record(&command, "api-cargo-private-baseline", receipt)?;
    crate::crypto_isolated::bounded_output(
        &mut command,
        "api-cargo-private-baseline",
        8 * 1024 * 1024,
        120,
    )?;
    let mut command = cargo_diagnostic("rustc", &manifest, "api_probe", true);
    private_dependencies(&mut command, "api_probe", &["api_probe_backend"]);
    crate::crypto_isolated::command_record(&command, "api-cargo-private", receipt)?;
    let output = crate::crypto_isolated::bounded_exit_output(
        &mut command,
        "api-cargo-private",
        8 * 1024 * 1024,
        120,
        101,
    )?;
    if !output.lines().any(|line| {
        crate::crypto_isolated::artifact_json(line)
            .ok()
            .is_some_and(|value| {
                value
                    .get("message")
                    .is_some_and(|value| compiler_error(value, "exported_private_dependencies"))
            })
    }) {
        return Err("Cargo API privacy pass did not reject the dyn-trait leak".into());
    }
    receipt.push_str("api-cargo-probes 3\n");
    Ok(())
}

fn unexpected_cfg_error(value: &Json) -> bool {
    compiler_error(value, "unexpected_cfgs")
}

fn compiler_error(value: &Json, code: &str) -> bool {
    value.get("level").and_then(Json::as_str) == Some("error")
        && value
            .get("code")
            .and_then(|v| v.get("code"))
            .and_then(Json::as_str)
            == Some(code)
}

const FIXTURES: &[(&str, &str, bool)] = &[
    (
        "private",
        "pub struct Opaque { inner: fake_backend::Key }",
        true,
    ),
    (
        "owned",
        "pub struct Opaque; pub trait T {type K;} impl T for Opaque {type K=Self;}",
        true,
    ),
    (
        "nested",
        "pub mod nested {pub use fake_backend::Key;}",
        false,
    ),
    ("renamed", "pub use fake_backend::Key as Renamed;", false),
    ("alias", "pub type Renamed = fake_backend::Key;", false),
    ("hidden", "#[doc(hidden)] pub use fake_backend::Key;", false),
    (
        "signature",
        "pub fn key()->fake_backend::Key {fake_backend::Key}",
        false,
    ),
    (
        "associated",
        "pub trait T {type K;} pub struct Opaque; impl T for Opaque {type K=fake_backend::Key;}",
        false,
    ),
    (
        "std_associated",
        "pub struct Opaque; impl Iterator for Opaque {type Item=fake_backend::Key; fn next(&mut self)->Option<Self::Item> {None}}",
        false,
    ),
    (
        "dyn_inherent",
        "pub trait Local {} impl dyn Local + Send {pub fn leak(&self)->fake_backend::Key {fake_backend::Key}}",
        false,
    ),
    (
        "dyn_trait_arg",
        "pub trait Local {} impl<'a> std::ops::Add<&'a dyn Local> for u32 {type Output=fake_backend::Key; fn add(self,_:&dyn Local)->Self::Output {fake_backend::Key}}",
        false,
    ),
    (
        "dyn_transitive",
        "pub trait Local {} impl dyn Local {pub fn leak(&self)->fake_backend::Leaf {fake_backend::Leaf}}",
        false,
    ),
    (
        "bound",
        "pub fn key<T:fake_backend::Foreign>(_:T) {}",
        false,
    ),
    (
        "method",
        "pub struct Opaque; impl Opaque {pub fn key(&self)->fake_backend::Key {fake_backend::Key}}",
        false,
    ),
    ("tuple", "pub struct Opaque(pub fake_backend::Key);", false),
    (
        "variant",
        "pub enum Opaque { Key { key:fake_backend::Key } }",
        false,
    ),
    (
        "constant",
        "pub const KEY:fake_backend::Key=fake_backend::Key;",
        false,
    ),
    (
        "union",
        "pub union Opaque {pub key:std::mem::ManuallyDrop<fake_backend::Key>}",
        false,
    ),
    (
        "cfg_not_doc",
        "#[cfg(not(doc))] pub use fake_backend::Key;",
        false,
    ),
    ("external_cfg", "fake_backend::expose!();", false),
    (
        "cfg_allow",
        "#![allow(unexpected_cfgs)] #[cfg(not(doc))] pub use fake_backend::Key;",
        false,
    ),
    (
        "cfg_attr",
        "#[cfg_attr(doc, cfg(any()))] pub use fake_backend::Key;",
        false,
    ),
    (
        "cfg_expression",
        "pub fn key()->[u8; cfg!(doc) as usize] {[]}",
        false,
    ),
    (
        "macro_cfg",
        "macro_rules! expose {()=>{#[cfg(not(doc))] pub use fake_backend::Key;}} expose!();",
        false,
    ),
    (
        "musl_cfg",
        "#[cfg(target_env=\"musl\")] pub use fake_backend::Key;",
        false,
    ),
    (
        "release_cfg",
        "#[cfg(not(debug_assertions))] pub use fake_backend::Key;",
        false,
    ),
    (
        "static_cfg",
        "#[cfg(target_feature=\"crt-static\")] pub use fake_backend::Key;",
        false,
    ),
    (
        "macro_export",
        "#[macro_export] macro_rules! key {()=>{fake_backend::Key};}",
        false,
    ),
    (
        "glob",
        "mod m {pub use fake_backend::Key;} pub use m::*;",
        false,
    ),
];

fn fixtures(root: &Path) -> Result<()> {
    let leaf = root.join("leaf.rs");
    fs::write(&leaf, "pub struct Leaf;").map_err(|e| e.to_string())?;
    let leaf_library = root.join("libfake_leaf.rlib");
    let mut command = compiler("rustc", "fake_leaf");
    command
        .args(["--crate-type", "lib"])
        .arg(&leaf)
        .arg("-o")
        .arg(&leaf_library);
    crate::crypto_isolated::bounded_output(&mut command, "api-fixture-leaf", 256 * 1024, 30)?;
    let backend = root.join("backend.rs");
    fs::write(&backend, "pub struct Key; pub use fake_leaf::Leaf; pub trait Foreign {} #[macro_export] macro_rules! expose {()=>{#[cfg(not(doc))] pub use fake_backend::Key;}}").map_err(|e| e.to_string())?;
    let library = root.join("libfake_backend.rlib");
    let mut command = compiler("rustc", "fake_backend");
    command
        .args(["--crate-type", "lib"])
        .arg("--extern")
        .arg(format!("fake_leaf={}", leaf_library.display()))
        .arg(&backend)
        .arg("-o")
        .arg(&library);
    crate::crypto_isolated::bounded_output(&mut command, "api-fixture-backend", 256 * 1024, 30)?;
    for (name, source, allowed) in FIXTURES {
        let directory = root.join(name);
        fs::create_dir(&directory).map_err(|e| e.to_string())?;
        let input = directory.join("lib.rs");
        fs::write(&input, source).map_err(|e| e.to_string())?;
        let mut command = compiler("rustc", "api_fixture");
        command
            .args(["--crate-type", "lib", "--extern"])
            .arg(format!("fake_backend={}", library.display()))
            .arg(&input)
            .arg("--emit=metadata")
            .arg("-o")
            .arg(directory.join("ordinary.rmeta"));
        crate::crypto_isolated::bounded_output(
            &mut command,
            &format!("api-{name}-ordinary"),
            256 * 1024,
            30,
        )?;
        let mut command = compiler("rustc", "api_fixture");
        command
            .args(CFG_GUARD)
            .args(["--crate-type", "lib", "--extern"])
            .arg(format!("fake_backend={}", library.display()))
            .arg(&input)
            .arg("--emit=metadata")
            .arg("-o")
            .arg(directory.join("guarded.rmeta"));
        let error_path = directory.join("guard.stderr");
        command
            .arg("--error-format=json")
            .stderr(fs::File::create(&error_path).map_err(|e| e.to_string())?);
        let divergent = matches!(
            *name,
            "cfg_not_doc"
                | "macro_cfg"
                | "external_cfg"
                | "cfg_allow"
                | "cfg_attr"
                | "cfg_expression"
                | "release_cfg"
        );
        crate::crypto_isolated::bounded_exit_output(
            &mut command,
            &format!("api-{name}-guard"),
            256 * 1024,
            30,
            i32::from(divergent),
        )?;
        if divergent {
            let errors =
                crate::crypto_isolated::read_output(&error_path, "API guard errors", 256 * 1024)?;
            if !errors.lines().any(|line| {
                let Ok(value) = crate::crypto_isolated::artifact_json(line) else {
                    return false;
                };
                unexpected_cfg_error(&value)
            }) {
                return Err(format!("API fixture {name} did not reject divergent cfg"));
            }
            continue;
        }
        let mut command = compiler("rustc", "api_fixture");
        command
            .args(["--crate-type", "lib", "--extern"])
            .arg(format!("fake_backend={}", library.display()))
            .arg(&input)
            .arg("--emit=metadata")
            .arg("-o")
            .arg(directory.join("private.rmeta"));
        private_dependencies(&mut command, "api_fixture", &["fake_backend"]);
        let error_path = directory.join("private.stderr");
        command
            .arg("--error-format=json")
            .stderr(fs::File::create(&error_path).map_err(|e| e.to_string())?);
        let foreign = !allowed && *name != "macro_export";
        crate::crypto_isolated::bounded_exit_output(
            &mut command,
            &format!("api-{name}-private"),
            256 * 1024,
            30,
            i32::from(foreign),
        )?;
        if foreign {
            let errors = crate::crypto_isolated::read_output(
                &error_path,
                "API private dependency errors",
                256 * 1024,
            )?;
            if !errors.lines().any(|line| {
                crate::crypto_isolated::artifact_json(line)
                    .ok()
                    .is_some_and(|value| compiler_error(&value, "exported_private_dependencies"))
            }) {
                return Err(format!(
                    "API fixture {name} did not reject private dependency"
                ));
            }
        }
        // Schema 57 omits these impls; rustc's privacy pass above is decisive.
        if matches!(*name, "dyn_inherent" | "dyn_trait_arg" | "dyn_transitive") {
            continue;
        }
        let mut command = diagnostic("api_fixture", &input, &directory, &library);
        crate::crypto_isolated::bounded_output(
            &mut command,
            &format!("api-{name}-doc"),
            256 * 1024,
            30,
        )?;
        let result = inspect_file(&directory.join("api_fixture.json"));
        match (allowed, result) {
            (true, Ok(_)) => {}
            (false, Err(error))
                if error.contains("exposes external crate fake_backend")
                    || (*name == "macro_export"
                        && error.contains("unsupported public API item macro")) => {}
            (_, result) => return Err(format!("API fixture {name} unexpected result: {result:?}")),
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn graph(export: &str, external: &str) -> Json {
        parse(&format!(
            r#"{{"format_version":57,"includes_private":true,"root":0,
            "target":{{"triple":"x86_64-unknown-linux-musl","target_features":[
                {{"name":"fxsr","globally_enabled":true}},{{"name":"sse","globally_enabled":true}},
                {{"name":"sse2","globally_enabled":true}},{{"name":"x87","globally_enabled":true}}]}},
            "index":{{"0":{{"crate_id":0,"inner":{{"module":{{"items":[1]}}}}}},
                "1":{{"crate_id":0,"visibility":"public","inner":{export}}}}},
            "paths":{{"2":{{"crate_id":1}}}},
            "external_crates":{{"1":{{"name":"{external}"}}}}}}"#
        ))
        .expect("fixture JSON")
    }

    #[test]
    fn follows_aliases_nested_types_and_rejects_unresolved_exports() {
        for export in [
            r#"{"use":{"id":2}}"#,
            r#"{"type_alias":{"type":{"resolved_path":{"id":2}}}}"#,
            r#"{"function":{"sig":{"inputs":[["key",{"resolved_path":{"id":2}}]]}}}"#,
            r#"{"assoc_type":{"type":{"resolved_path":{"id":2}}}}"#,
        ] {
            assert!(inspect(&graph(export, "rustls"))
                .unwrap_err()
                .contains("exposes external crate rustls"));
            assert!(inspect(&graph(export, "std")).is_ok());
        }
        for export in [
            r#"{"use":{"id":null}}"#,
            r#"{"use":{"id":99}}"#,
            r#"{"macro":"..."}"#,
            r#"{"new_kind":{}}"#,
        ] {
            assert!(inspect(&graph(export, "std")).is_err());
        }
    }

    #[test]
    fn refuses_changed_schema_missing_private_items_and_duplicate_ids() {
        let base = graph(r#"{"use":{"id":2}}"#, "std").to_canonical();
        for altered in [
            base.replace("\"format_version\":57", "\"format_version\":58"),
            base.replace("\"includes_private\":true", "\"includes_private\":false"),
            base.replace("\"root\":0", "\"root\":99"),
            base.replace("x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu"),
            base.replace("\"globally_enabled\":true", "\"globally_enabled\":false"),
            base.replace("\"paths\":{", "\"paths\":{\"2\":{},"),
        ] {
            assert!(parse(&altered)
                .and_then(|document| inspect(&document))
                .is_err());
        }
    }

    #[test]
    fn bounds_json_before_recursive_parser() {
        let nested = format!("{}0{}", "[".repeat(129), "]".repeat(129));
        assert!(parse(&nested).unwrap_err().contains("nesting"));
        assert!(parse(&" ".repeat(16 * 1024 * 1024 + 1))
            .unwrap_err()
            .contains("16 MiB"));
        assert!(parse("}").is_err());
        assert!(parse(r#"["{[\\\"", 0]"#).is_ok());
    }

    #[test]
    fn cargo_cannot_merge_away_cfg_guard_through_manifest_overrides() {
        let admitted = crate::crypto_policy::DEPENDENCIES
            .iter()
            .map(|line| {
                line.split_once(' ')
                    .expect("dependency name")
                    .0
                    .replace('-', "_")
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            admitted,
            PRIVATE_DEPENDENCIES
                .iter()
                .map(|name| (*name).to_owned())
                .collect()
        );
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("repository");
        for name in ["td-crypto", "td-mta"] {
            let original =
                fs::read_to_string(root.join(name).join("Cargo.toml")).expect("manifest");
            crate::crypto_policy::manifest_pin(name, &original).expect("admitted manifest");
            for suffix in [
                "\n[lints.rust]\nunexpected_cfgs = { level = \"warn\", check-cfg = [\"cfg(doc)\"] }\n",
                "\n[profile.release]\ndebug-assertions = true\n",
            ] {
                assert!(crate::crypto_policy::manifest_pin(name, &format!("{original}{suffix}")).is_err());
            }
            let scripted =
                original.replacen("[package]", "[package]\nbuild = \"custom-build.rs\"", 1);
            assert!(crate::crypto_policy::manifest_pin(name, &scripted).is_err());
        }
    }
}
