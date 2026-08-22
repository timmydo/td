//! The shell's function table, and the one rule that separates naming a
//! function from resolving a command word to one.
//!
//! ash keeps functions in the same `cmdtable` it keeps hashed paths and
//! builtins in, and applies the rule where it RESOLVES a word rather than
//! where it files one: `find_command` (ash.c:13788) returns `CMDNORMAL` for a
//! name containing `/` at 13801-13815 -- "don't use PATH or hash table" --
//! above the `cmdlookup` at 13825 that finds a `CMDFUNCTION`. So `a/b() { ...
//! }` is defined, is offered by completion, and answers `a/b` with a path
//! lookup that misses.
//!
//! The table lives behind this type, in a module of its own: a private field
//! on `Shell` would still be reachable from `exec`, where the frame decision
//! is. What the compiler then enforces is that nothing written OUTSIDE this
//! module can obtain an `&Func` except through `get`; nothing constrains
//! `funcs.rs` itself, where a fifth accessor or a `Deref` impl hands one out
//! with no rule applied. That `defined_names` is not turned into a lookup --
//! enumerating answers a word question too -- is pinned by a test on the
//! modules that name it, which a forwarder can still walk around. The commit
//! message measures all three gaps: the in-module one, the forwarder, and a
//! decoy comment against the declaration pin.

use crate::exec::Func;
use std::collections::HashMap;

/// A shell's functions by name.
#[derive(Clone, Default)]
pub struct Funcs {
    /// Private to this module. Widening it to `pub` would put every reader
    /// back in a position to answer a lookup without the rule, and to do it
    /// with no observable; the widening itself reds the declaration pin.
    table: HashMap<String, Func>,
}

impl Funcs {
    /// What a command WORD names, which is not every name in the table: a `/`
    /// makes the word a path in ash and so here.
    pub fn get(&self, name: &str) -> Option<&Func> {
        if name.contains('/') {
            return None;
        }
        self.table.get(name)
    }

    /// File a definition. ash's `defun` (ash.c:9217) calls `addcmdentry` with
    /// no name check at all, so any name a definition can carry is stored --
    /// including one `get` will never answer to.
    pub fn define(&mut self, name: String, func: Func) {
        self.table.insert(name, func);
    }

    /// `unset -f`, which reaches the TABLE and not a word: ash's `unsetfunc`
    /// (ash.c:14181) looks the name up with a bare `cmdlookup`, so a `/` name
    /// it can never call is one it can still remove.
    pub fn remove(&mut self, name: &str) {
        self.table.remove(name);
    }

    /// Every name filed, for completion. ash's `ash_command_name`
    /// (ash.c:10300) walks `cmdtable` for `CMDFUNCTION` entries and offers
    /// what it finds, so a name that cannot be called is still a name that
    /// is offered. Spelled distinctly because the confinement test pins its
    /// callers by this identifier: a generic one would both miss and be
    /// missed.
    pub fn defined_names(&self) -> impl Iterator<Item = &String> {
        self.table.keys()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Cmd, List, Stage};
    use std::sync::Arc;

    fn func() -> Func {
        Func {
            line: 1,
            body: Arc::new(Stage {
                line: 1,
                cmd: Cmd::Subshell { body: List { items: Vec::new() }, redirs: Vec::new() },
            }),
        }
    }

    /// The asymmetry the whole type exists for: a `/` name is filed, is
    /// enumerated, and is removable, and is not what any word resolves to.
    #[test]
    fn a_slash_name_is_filed_and_enumerated_but_never_resolved() {
        let mut funcs = Funcs::default();
        funcs.define("a/b".to_string(), func());
        funcs.define("ab".to_string(), func());
        assert!(funcs.get("a/b").is_none(), "a `/` name is not what a word names");
        assert!(funcs.get("ab").is_some(), "and every other name still is");
        let mut names: Vec<&String> = funcs.defined_names().collect();
        names.sort();
        assert_eq!(names, ["a/b", "ab"], "both are filed and both are offered");
        funcs.remove("a/b");
        let names: Vec<&String> = funcs.defined_names().collect();
        assert_eq!(names, ["ab"], "`unset -f` reaches the one `get` will not");
    }
}
