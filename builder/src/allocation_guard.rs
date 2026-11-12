//! A source-level allocation debt ratchet, not a type/allocator proof.
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

type Inventory = BTreeMap<(String, String), usize>;
const INVENTORY: &str = "td-alloc/allocation-debt.tsv";

fn operations(source: &str) -> BTreeMap<String, usize> {
    let code =
        td_engine::embed_scan::blank_literals(&td_engine::embed_scan::production_code(source));
    let mut tokens = Vec::new();
    let mut word = String::new();
    for c in code.chars() {
        if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            if !word.is_empty() {
                tokens.push(std::mem::take(&mut word));
            }
            if !c.is_whitespace() {
                tokens.push(c.to_string());
            }
        }
    }
    if !word.is_empty() {
        tokens.push(word);
    }
    let mut counts = BTreeMap::new();
    for (i, token) in tokens.iter().enumerate() {
        let before = i
            .checked_sub(1)
            .and_then(|n| tokens.get(n))
            .map(String::as_str);
        let next = tokens.get(i + 1).map(String::as_str);
        let category = if ["alloc", "alloc_zeroed", "realloc", "dealloc"].contains(&token.as_str())
            && next == Some("(")
            && before != Some(".")
        {
            Some("raw-allocator")
        } else if next == Some("!") && ["vec", "format"].contains(&token.as_str()) {
            Some("allocation-macro")
        } else if before == Some(".")
            && [
                "collect",
                "to_vec",
                "to_owned",
                "to_string",
                "repeat",
                "concat",
                "join",
                "into_owned",
                "replace",
                "replacen",
                "to_lowercase",
                "to_uppercase",
                "into_boxed_slice",
                "into_boxed_str",
                "to_path_buf",
            ]
            .contains(&token.as_str())
            && matches!(next, Some("(" | ":"))
        {
            Some("allocating-conversion")
        } else if before == Some(".")
            && [
                "reserve",
                "reserve_exact",
                "shrink_to_fit",
                "shrink_to",
                "push",
                "push_front",
                "push_back",
                "push_str",
                "insert",
                "extend",
                "extend_from_slice",
                "resize",
                "resize_with",
                "append",
                "split_off",
                "clone",
                "clone_from",
                "sort",
                "sort_by",
                "sort_by_key",
            ]
            .contains(&token.as_str())
            && matches!(next, Some("(" | ":"))
        {
            Some("unchecked-growth-or-clone")
        } else if before == Some(".")
            && ["try_reserve", "try_reserve_exact"].contains(&token.as_str())
        {
            Some("direct-reservation")
        } else if before == Some(":")
            && !i
                .checked_sub(3)
                .and_then(|n| tokens.get(n))
                .is_some_and(|owner| ["td_alloc", "allocation"].contains(&owner.as_str()))
            && ["with_capacity", "from_iter"].contains(&token.as_str())
        {
            Some("allocating-constructor")
        } else if ["Box", "Arc", "Rc", "String", "Vec"].contains(&token.as_str())
            && next == Some(":")
        {
            // Include turbofish forms; stop at the first expression delimiter.
            let mut j = i + 1;
            let mut found = false;
            while let Some(part) = tokens.get(j) {
                if ["(", ";", "{", "}"].contains(&part.as_str()) {
                    break;
                }
                if (part == "from" || part == "from_iter")
                    || (["Box", "Arc", "Rc"].contains(&token.as_str())
                        && [
                            "new",
                            "new_cyclic",
                            "new_uninit",
                            "new_zeroed",
                            "new_uninit_slice",
                            "new_zeroed_slice",
                        ]
                        .contains(&part.as_str()))
                {
                    found = true;
                    break;
                }
                j += 1;
            }
            found.then_some("heap-owner")
        } else {
            None
        };
        if let Some(category) = category {
            *counts.entry(category.to_string()).or_insert(0) += 1;
        }
    }
    counts
}

fn walk(root: &Path, path: &Path, inventory: &mut Inventory) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "allocation scan refuses symlink {}",
            path.display()
        ));
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            if matches!(
                name.to_str(),
                Some("target" | "tests" | "benches" | "examples" | ".git" | ".td-build-cache")
            ) {
                continue;
            }
            walk(root, &entry.path(), inventory)?;
        }
    } else if path.extension().is_some_and(|e| e == "rs") {
        let relative = path
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_str()
            .ok_or("non-UTF-8 allocation scan path")?
            .to_string();
        let source = fs::read_to_string(path).map_err(|e| format!("{relative}: {e}"))?;
        for (operation, count) in operations(&source) {
            inventory.insert((relative.clone(), operation), count);
        }
    }
    Ok(())
}

fn inventory(root: &Path) -> Result<Inventory, String> {
    let mut out = Inventory::new();
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err("non-UTF-8 crate name".into());
        };
        if ["builder", "recipes", "engine", "net"].contains(&name) || name.starts_with("td-") {
            let path = entry.path();
            if path.join("Cargo.toml").is_file() {
                walk(root, &path, &mut out)?;
            }
        }
    }
    if out.is_empty() {
        return Err("allocation inventory is empty".into());
    }
    Ok(out)
}

pub(crate) fn render(root: &Path) -> Result<String, String> {
    let mut text = String::from(
        "# Production allocation debt: path, operation, count. See td-alloc/DESIGN.md.\n",
    );
    for ((path, operation), count) in inventory(root)? {
        use std::fmt::Write;
        writeln!(text, "{path}\t{operation}\t{count}").map_err(|e| e.to_string())?;
    }
    Ok(text)
}

fn parse(text: &str) -> Result<Inventory, String> {
    let mut out = Inventory::new();
    for line in text
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
    {
        let fields: Vec<_> = line.split('\t').collect();
        let [path, operation, count] = fields.as_slice() else {
            return Err("malformed allocation inventory row".into());
        };
        let count = count
            .parse::<usize>()
            .map_err(|_| "invalid allocation inventory count")?;
        if count == 0
            || out
                .insert((path.to_string(), operation.to_string()), count)
                .is_some()
        {
            return Err("duplicate or zero allocation inventory row".into());
        }
    }
    Ok(out)
}

pub(crate) fn check(root: &Path) -> Result<(), String> {
    check_owned_boundary(root)?;
    let recorded =
        fs::read_to_string(root.join(INVENTORY)).map_err(|e| format!("{INVENTORY}: {e}"))?;
    compare(&parse(&recorded)?, &inventory(root)?)
}

fn check_owned_boundary(root: &Path) -> Result<(), String> {
    let source = root.join("td-alloc/src");
    let expected = ["budget.rs", "lib.rs", "owned.rs", "raw.rs", "storage.rs"];
    let mut seen = Vec::new();
    for entry in fs::read_dir(&source).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
            return Err("td-alloc: unexpected directory or symlink in allocation boundary".into());
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "td-alloc: non-UTF-8 source")?;
        if !expected.contains(&name.as_str()) {
            return Err(format!("td-alloc: unreviewed source {name}"));
        }
        let text = fs::read_to_string(entry.path()).map_err(|e| e.to_string())?;
        owned_source(&name, &text)?;
        seen.push(name);
    }
    if seen.len() != expected.len() {
        return Err("td-alloc: missing allocation source".into());
    }
    let manifest =
        fs::read_to_string(root.join("td-alloc/Cargo.toml")).map_err(|e| e.to_string())?;
    if !manifest.contains("path = \"src/owned.rs\"") || root.join("td-alloc/build.rs").exists() {
        return Err("td-alloc: allocation root or build-script boundary changed".into());
    }
    Ok(())
}

fn owned_source(name: &str, source: &str) -> Result<(), String> {
    let code =
        td_engine::embed_scan::blank_literals(&td_engine::embed_scan::production_code(source));
    let compact: String = code.split_whitespace().collect();
    let unsafe_words = code
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| *word == "unsafe")
        .count();
    let (words, blocks, allows) = if name == "raw.rs" {
        (6, 4, 6)
    } else {
        (0, 0, 0)
    };
    if unsafe_words != words
        || compact.matches("unsafe{").count() != blocks
        || compact.matches("allow(unsafe_code)").count() != allows
        || compact.contains("include!")
        || compact.contains("include_bytes!")
        || compact.contains("include_str!")
        || compact.contains("cfg_attr")
        || compact.contains("global_allocator")
        || (name != "owned.rs" && compact.contains("#[path"))
    {
        return Err(format!(
            "td-alloc/{name}: allocation confinement changed; review UNSAFE.md section 28"
        ));
    }
    if name == "owned.rs" {
        // The literal path is intentionally checked before blanking literals.
        if !source.contains("#[path = \"lib.rs\"]")
            || compact.matches("#[path").count() != 1
            || !compact.contains("#![deny(unsafe_code)]")
        {
            return Err("td-alloc: owned root confinement changed".into());
        }
    }
    Ok(())
}

fn compare(recorded: &Inventory, actual: &Inventory) -> Result<(), String> {
    for ((path, operation), count) in actual {
        let allowed = recorded
            .get(&(path.clone(), operation.clone()))
            .copied()
            .unwrap_or(0);
        if *count > allowed {
            return Err(format!("{path}: {operation} increased from {allowed} to {count}; use td-alloc or review a specific debt amendment"));
        }
    }
    for (key, allowed) in recorded {
        if actual.get(key) != Some(allowed) {
            return Err(format!(
                "{}: stale {} allocation debt; reduce the inventory",
                key.0, key.1
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn owned_allocation_boundary_rejects_new_unsafe_and_external_source() {
        owned_source("budget.rs", "fn safe() {}").unwrap();
        for source in [
            "unsafe fn added() {}",
            "include!(\"other.rs\");",
            "#[allow(unsafe_code)] fn added() {}",
            "#[path=\"other.rs\"] mod x;",
            "#[global_allocator] static A: A = A;",
        ] {
            assert!(owned_source("budget.rs", source).is_err());
        }
        assert_eq!(
            operations("std::alloc::alloc(layout); alloc_zeroed(layout); realloc(p, l, n);")
                .get("raw-allocator"),
            Some(&3)
        );
    }

    #[test]
    fn code_only_scan_covers_spacing_generics_and_macros() {
        let counts = operations(
            r###"
            // vec![1]; Box::new(1)
            const S: &str = r#"format!("x"); value.push(1)"#;
            fn live() { let a = std::vec ! [0; 4]; let b = Vec::<u8>::with_capacity(4);
                let c = std::sync::Arc::<u8>::new(3); xs . collect :: < Vec<_> > (); }
            #[cfg(test)] mod tests { fn ignored() { vec![0]; } }
        "###,
        );
        assert_eq!(counts.get("allocation-macro"), Some(&1));
        assert_eq!(counts.get("allocating-constructor"), Some(&1));
        assert_eq!(counts.get("heap-owner"), Some(&1));
        assert_eq!(counts.get("allocating-conversion"), Some(&1));
        assert_eq!(counts.len(), 4);
    }

    #[test]
    fn raw_byte_and_c_literals_cannot_hide_following_allocations() {
        for prefix in ["br", "cr", "r"] {
            let source = format!(
                "fn data() {{ let _data = {prefix}#\"a \" b /*\"#; }}\nfn live() -> Vec<u8> {{ vec![0; 10] }}\nconst END: &str = \"done\";"
            );
            assert_eq!(
                operations(&source).get("allocation-macro"),
                Some(&1),
                "{prefix}"
            );
            let source = format!(
                "#[cfg(test)]\nmod tests {{ fn data() {{ let _data = {prefix}#\"a \" }} /*\"#; }} }}\nfn live() -> Vec<u8> {{ vec![0; 10] }}"
            );
            assert_eq!(
                operations(&source).get("allocation-macro"),
                Some(&1),
                "{prefix}"
            );
        }
    }

    #[test]
    fn growth_new_files_and_stale_debt_are_rejected() {
        let old = parse("a.rs\tallocation-macro\t1\n").unwrap();
        assert!(compare(&old, &old).is_ok());
        assert!(compare(&old, &parse("a.rs\tallocation-macro\t2\n").unwrap()).is_err());
        assert!(compare(&old, &parse("b.rs\tallocation-macro\t1\n").unwrap()).is_err());
        assert!(compare(&old, &Inventory::new()).is_err());
        assert!(parse("a.rs\tx\t1\na.rs\tx\t1\n").is_err());
    }

    #[test]
    fn discovery_checks_untracked_sources_and_refuses_missing_inventory() {
        let temp = crate::test_temp::TempDir::new("allocation-guard").unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("td-example/src")).unwrap();
        fs::create_dir_all(root.join("td-alloc/src")).unwrap();
        fs::write(
            root.join("td-alloc/Cargo.toml"),
            include_str!("../../td-alloc/Cargo.toml"),
        )
        .unwrap();
        for (name, text) in [
            ("owned.rs", include_str!("../../td-alloc/src/owned.rs")),
            ("lib.rs", include_str!("../../td-alloc/src/lib.rs")),
            ("raw.rs", include_str!("../../td-alloc/src/raw.rs")),
            ("budget.rs", include_str!("../../td-alloc/src/budget.rs")),
            ("storage.rs", include_str!("../../td-alloc/src/storage.rs")),
        ] {
            fs::write(root.join("td-alloc/src").join(name), text).unwrap();
        }
        fs::write(
            root.join("td-example/Cargo.toml"),
            "[package]\nname = \"td-example\"\n",
        )
        .unwrap();
        fs::write(
            root.join("td-example/src/lib.rs"),
            "fn f() { let x = vec![1]; }",
        )
        .unwrap();
        assert!(check(root).is_err());
        fs::write(root.join(INVENTORY), render(root).unwrap()).unwrap();
        assert!(check(root).is_ok());
        fs::write(
            root.join("td-example/src/new.rs"),
            "fn g() { let s = format!(\"x\"); }",
        )
        .unwrap();
        assert!(check(root).unwrap_err().contains("new.rs"));
    }
}
