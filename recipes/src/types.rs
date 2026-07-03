//! The recipe vocabulary as TYPED Rust — a faithful mirror of `tests/ts/td-spec.d.ts`.
//!
//! This is the whole point of moving the package surface off boa/TypeScript: the
//! union types (`BuildSystem`, the `Replacement`/`FileArg`/`Stmt` sums) become
//! Rust enums and the shapes become structs, so `rustc` enforces at compile time
//! exactly what `tsc` enforced via the ambient `.d.ts` — a malformed recipe does
//! not compile, the same property the `ts`/`tsgo-pin` gates buy today. Each type
//! carries a `to_json` producing the SAME JSON shape boa emitted, so the Guile
//! lowering bridge is unchanged (camelCase keys; an optional field is emitted iff
//! it is present, matching boa's "keys present in the object literal").

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unreachable, clippy::todo, clippy::unimplemented, clippy::indexing_slicing)] // grandfathered: pre-dates the rust-lint rules (AGENTS.md); remove when cleaned

use crate::json::Json;

fn vs(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|x| x.to_string()).collect()
}

fn arr(xs: &[String]) -> Json {
    Json::Arr(xs.iter().map(|x| Json::Str(x.clone())).collect())
}

/// Build systems td knows how to lower (mirrors `BuildSystem` in td-spec.d.ts).
#[derive(Clone)]
pub enum BuildSystem {
    Gnu,
    Rust,
    Cmake,
}

impl BuildSystem {
    fn as_str(&self) -> &'static str {
        match self {
            BuildSystem::Gnu => "gnu",
            BuildSystem::Rust => "rust",
            BuildSystem::Cmake => "cmake",
        }
    }
}

/// An upstream source URI — a single URL or a list of mirror URLs (these lower to
/// DIFFERENT source derivations, so the shape is load-bearing — `Source` union).
#[derive(Clone)]
pub enum Uri {
    One(String),
    List(Vec<String>),
}

impl Uri {
    fn to_json(&self) -> Json {
        match self {
            Uri::One(u) => Json::Str(u.clone()),
            Uri::List(us) => Json::Arr(us.iter().map(|u| Json::Str(u.clone())).collect()),
        }
    }
}

/// An upstream source: a URI (or mirror list) + its nix-base32 sha256.
#[derive(Clone)]
pub struct Source {
    pub uri: Uri,
    pub sha256: String,
}

impl Source {
    pub fn one(uri: &str, sha256: &str) -> Source {
        Source {
            uri: Uri::One(uri.into()),
            sha256: sha256.into(),
        }
    }
    pub fn list(uris: &[&str], sha256: &str) -> Source {
        Source {
            uri: Uri::List(vs(uris)),
            sha256: sha256.into(),
        }
    }
    fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("uri".into(), self.uri.to_json()),
            ("sha256".into(), Json::Str(self.sha256.clone())),
        ])
    }
}

/// A part of a `string-append`/`format` replacement (`RefPart` in td-spec.d.ts).
#[derive(Clone)]
pub enum RefPart {
    Lit(String),
    Output(String),
    Input(String),
    Var(String),
}

impl RefPart {
    fn to_json(&self) -> Json {
        match self {
            RefPart::Lit(x) => Json::Str(x.clone()),
            RefPart::Output(x) => Json::Obj(vec![("output".into(), Json::Str(x.clone()))]),
            RefPart::Input(x) => Json::Obj(vec![("input".into(), Json::Str(x.clone()))]),
            RefPart::Var(x) => Json::Obj(vec![("var".into(), Json::Str(x.clone()))]),
        }
    }
}

/// A `substitute*` replacement (`Replacement` union in td-spec.d.ts).
#[derive(Clone)]
pub enum Replacement {
    Lit(String),
    Var(String),
    Which(String),
    StringAppend(Vec<RefPart>),
    /// `{ format: [FMT, PART…] }`.
    Format(String, Vec<RefPart>),
}

impl Replacement {
    fn to_json(&self) -> Json {
        match self {
            Replacement::Lit(x) => Json::Str(x.clone()),
            Replacement::Var(x) => Json::Obj(vec![("var".into(), Json::Str(x.clone()))]),
            Replacement::Which(x) => Json::Obj(vec![("which".into(), Json::Str(x.clone()))]),
            Replacement::StringAppend(parts) => Json::Obj(vec![(
                "stringAppend".into(),
                Json::Arr(parts.iter().map(|p| p.to_json()).collect()),
            )]),
            Replacement::Format(fmt, parts) => {
                let mut a = vec![Json::Str(fmt.clone())];
                a.extend(parts.iter().map(|p| p.to_json()));
                Json::Obj(vec![("format".into(), Json::Arr(a))])
            }
        }
    }
}

/// A `substitute*` FILE argument (`FileArg` union in td-spec.d.ts).
#[derive(Clone)]
pub enum FileArg {
    Lit(String),
    List(Vec<String>),
    FindFiles(String, String),
    Cons(Box<FileArg>, Box<FileArg>),
}

impl FileArg {
    fn to_json(&self) -> Json {
        match self {
            FileArg::Lit(x) => Json::Str(x.clone()),
            FileArg::List(xs) => Json::Obj(vec![(
                "list".into(),
                Json::Arr(xs.iter().map(|x| Json::Str(x.clone())).collect()),
            )]),
            FileArg::FindFiles(d, r) => Json::Obj(vec![(
                "findFiles".into(),
                Json::Arr(vec![Json::Str(d.clone()), Json::Str(r.clone())]),
            )]),
            FileArg::Cons(a, b) => Json::Obj(vec![(
                "cons".into(),
                Json::Arr(vec![a.to_json(), b.to_json()]),
            )]),
        }
    }
}

/// One `substitute*` clause `((FROM MATCH-VAR…) TO)` (`Clause` in td-spec.d.ts).
#[derive(Clone)]
pub struct Clause {
    pub from: String,
    pub matches: Option<Vec<String>>,
    pub to: Replacement,
}

impl Clause {
    pub fn new(from: &str, to: Replacement) -> Clause {
        Clause {
            from: from.into(),
            matches: None,
            to,
        }
    }
    pub fn matching(mut self, xs: &[&str]) -> Clause {
        self.matches = Some(vs(xs));
        self
    }
    fn to_json(&self) -> Json {
        let mut o = vec![("from".into(), Json::Str(self.from.clone()))];
        if let Some(m) = &self.matches {
            o.push(("match".into(), arr(m)));
        }
        o.push(("to".into(), self.to.to_json()));
        Json::Obj(o)
    }
}

/// A phase-body statement (`Stmt` union in td-spec.d.ts).
#[derive(Clone)]
pub enum Stmt {
    Substitute {
        file: FileArg,
        clauses: Vec<Clause>,
    },
    LetWhich {
        binds: Vec<(String, String)>,
        body: Vec<Stmt>,
    },
    WithDefaultPortEncodingFalse {
        body: Vec<Stmt>,
    },
}

impl Stmt {
    fn to_json(&self) -> Json {
        match self {
            Stmt::Substitute { file, clauses } => Json::Obj(vec![
                ("substitute".into(), file.to_json()),
                (
                    "clauses".into(),
                    Json::Arr(clauses.iter().map(|c| c.to_json()).collect()),
                ),
            ]),
            Stmt::LetWhich { binds, body } => Json::Obj(vec![
                (
                    "letWhich".into(),
                    Json::Arr(
                        binds
                            .iter()
                            .map(|(n, p)| {
                                Json::Obj(vec![
                                    ("name".into(), Json::Str(n.clone())),
                                    ("prog".into(), Json::Str(p.clone())),
                                ])
                            })
                            .collect(),
                    ),
                ),
                (
                    "body".into(),
                    Json::Arr(body.iter().map(|s| s.to_json()).collect()),
                ),
            ]),
            Stmt::WithDefaultPortEncodingFalse { body } => Json::Obj(vec![
                ("withDefaultPortEncodingFalse".into(), Json::Bool(true)),
                (
                    "body".into(),
                    Json::Arr(body.iter().map(|s| s.to_json()).collect()),
                ),
            ]),
        }
    }
}

/// A flat `substitute*` over one source file (`Substitution` in td-spec.d.ts).
#[derive(Clone)]
pub struct Substitution {
    pub file: String,
    pub from: String,
    pub to: Replacement,
}

impl Substitution {
    pub fn new(file: &str, from: &str, to: Replacement) -> Substitution {
        Substitution {
            file: file.into(),
            from: from.into(),
            to,
        }
    }
    fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("file".into(), Json::Str(self.file.clone())),
            ("from".into(), Json::Str(self.from.clone())),
            ("to".into(), self.to.to_json()),
        ])
    }
}

/// A custom build phase (`Phase` in td-spec.d.ts).
#[derive(Clone)]
pub struct Phase {
    pub position: String,
    pub anchor: String,
    pub name: String,
    pub lambda_args: Option<Vec<String>>,
    pub substitutions: Option<Vec<Substitution>>,
    pub return_true: Option<bool>,
    pub body: Option<Vec<Stmt>>,
}

impl Phase {
    pub fn new(position: &str, anchor: &str, name: &str) -> Phase {
        Phase {
            position: position.into(),
            anchor: anchor.into(),
            name: name.into(),
            lambda_args: None,
            substitutions: None,
            return_true: None,
            body: None,
        }
    }
    pub fn lambda_args(mut self, xs: &[&str]) -> Phase {
        self.lambda_args = Some(vs(xs));
        self
    }
    pub fn substitutions(mut self, xs: Vec<Substitution>) -> Phase {
        self.substitutions = Some(xs);
        self
    }
    pub fn return_true(mut self) -> Phase {
        self.return_true = Some(true);
        self
    }
    pub fn body(mut self, xs: Vec<Stmt>) -> Phase {
        self.body = Some(xs);
        self
    }
    fn to_json(&self) -> Json {
        let mut o = vec![
            ("position".into(), Json::Str(self.position.clone())),
            ("anchor".into(), Json::Str(self.anchor.clone())),
            ("name".into(), Json::Str(self.name.clone())),
        ];
        if let Some(la) = &self.lambda_args {
            o.push(("lambdaArgs".into(), arr(la)));
        }
        if let Some(subs) = &self.substitutions {
            o.push((
                "substitutions".into(),
                Json::Arr(subs.iter().map(|s| s.to_json()).collect()),
            ));
        }
        if let Some(rt) = self.return_true {
            o.push(("returnTrue".into(), Json::Bool(rt)));
        }
        if let Some(body) = &self.body {
            o.push((
                "body".into(),
                Json::Arr(body.iter().map(|s| s.to_json()).collect()),
            ));
        }
        Json::Obj(o)
    }
}

/// A package recipe — the coordinates that determine the build derivation
/// (`Recipe` in td-spec.d.ts). Built with the `gnu`/`rust`/`cmake` constructors
/// plus chained setters; an unset optional field is omitted from the JSON.
#[derive(Clone)]
pub struct Recipe {
    pub name: String,
    pub version: String,
    pub source: Option<Source>,
    pub build_system: BuildSystem,
    pub inputs: Option<Vec<String>>,
    pub configure_flags: Option<Vec<String>>,
    pub make_flags: Option<Vec<String>>,
    pub outputs: Option<Vec<String>>,
    pub phases: Option<Vec<Phase>>,
    pub tests: Option<bool>,
    pub bins: Option<Vec<String>>,
    pub no_default_features: Option<bool>,
    pub features: Option<Vec<String>>,
}

impl Recipe {
    fn base(name: &str, version: &str, bs: BuildSystem) -> Recipe {
        Recipe {
            name: name.into(),
            version: version.into(),
            source: None,
            build_system: bs,
            inputs: None,
            configure_flags: None,
            make_flags: None,
            outputs: None,
            phases: None,
            tests: None,
            bins: None,
            no_default_features: None,
            features: None,
        }
    }
    pub fn gnu(name: &str, version: &str) -> Recipe {
        Recipe::base(name, version, BuildSystem::Gnu)
    }
    pub fn rust(name: &str, version: &str) -> Recipe {
        Recipe::base(name, version, BuildSystem::Rust)
    }
    pub fn cmake(name: &str, version: &str) -> Recipe {
        Recipe::base(name, version, BuildSystem::Cmake)
    }

    pub fn source(mut self, src: Source) -> Recipe {
        self.source = Some(src);
        self
    }
    pub fn inputs(mut self, xs: &[&str]) -> Recipe {
        self.inputs = Some(vs(xs));
        self
    }
    pub fn configure_flags(mut self, xs: &[&str]) -> Recipe {
        self.configure_flags = Some(vs(xs));
        self
    }
    pub fn make_flags(mut self, xs: &[&str]) -> Recipe {
        self.make_flags = Some(vs(xs));
        self
    }
    pub fn outputs(mut self, xs: &[&str]) -> Recipe {
        self.outputs = Some(vs(xs));
        self
    }
    pub fn phases(mut self, p: Vec<Phase>) -> Recipe {
        self.phases = Some(p);
        self
    }
    pub fn tests(mut self, t: bool) -> Recipe {
        self.tests = Some(t);
        self
    }
    pub fn bins(mut self, xs: &[&str]) -> Recipe {
        self.bins = Some(vs(xs));
        self
    }
    pub fn no_default_features(mut self) -> Recipe {
        self.no_default_features = Some(true);
        self
    }
    pub fn features(mut self, xs: &[&str]) -> Recipe {
        self.features = Some(vs(xs));
        self
    }

    /// The build system as its JSON/lowering token ("gnu"/"rust"/"cmake").
    pub fn build_system_name(&self) -> &'static str {
        self.build_system.as_str()
    }

    pub fn to_json(&self) -> Json {
        let mut o = vec![
            ("name".into(), Json::Str(self.name.clone())),
            ("version".into(), Json::Str(self.version.clone())),
        ];
        if let Some(src) = &self.source {
            o.push(("source".into(), src.to_json()));
        }
        o.push((
            "buildSystem".into(),
            Json::Str(self.build_system.as_str().into()),
        ));
        if let Some(x) = &self.inputs {
            o.push(("inputs".into(), arr(x)));
        }
        if let Some(x) = &self.configure_flags {
            o.push(("configureFlags".into(), arr(x)));
        }
        if let Some(x) = &self.make_flags {
            o.push(("makeFlags".into(), arr(x)));
        }
        if let Some(x) = &self.outputs {
            o.push(("outputs".into(), arr(x)));
        }
        if let Some(x) = &self.phases {
            o.push((
                "phases".into(),
                Json::Arr(x.iter().map(|p| p.to_json()).collect()),
            ));
        }
        if let Some(t) = self.tests {
            o.push(("tests".into(), Json::Bool(t)));
        }
        if let Some(x) = &self.bins {
            o.push(("bins".into(), arr(x)));
        }
        if let Some(b) = self.no_default_features {
            o.push(("noDefaultFeatures".into(), Json::Bool(b)));
        }
        if let Some(x) = &self.features {
            o.push(("features".into(), arr(x)));
        }
        Json::Obj(o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_recipe_emits_expected_shape() {
        let r = Recipe::gnu("hello", "2.12.2").source(Source::one(
            "mirror://gnu/hello/hello-2.12.2.tar.gz",
            "1aqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js",
        ));
        assert_eq!(
            r.to_json().to_canonical(),
            r#"{"buildSystem":"gnu","name":"hello","source":{"sha256":"1aqq1379syjckf0wdn9vs6wfbapnj9zfikhiykf29k4jq9nrk6js","uri":"mirror://gnu/hello/hello-2.12.2.tar.gz"},"version":"2.12.2"}"#
        );
    }

    #[test]
    fn optional_fields_are_omitted_when_unset() {
        let r = Recipe::rust("cat", "0.9.0").bins(&["cat"]);
        // no source / inputs / tests keys
        assert_eq!(
            r.to_json().to_canonical(),
            r#"{"bins":["cat"],"buildSystem":"rust","name":"cat","version":"0.9.0"}"#
        );
    }
}
