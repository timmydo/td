//! The Debian build root's lock (`recipes/src/debian-root.lock`,
//! BUILD-ROOT-PLAN.md): its pinned image layer and .debs, in unpack order,
//! and the NAR hash of the tree they assemble into. Shared by td-recipe-eval,
//! which lists the pins for caching, and td-builder, which assembles and
//! authenticates the tree. `td-feed debian-root resolve` writes the pin rows;
//! `td-builder build-root pin` writes the `tree` row.

/// Which part of the root a pin is: the image layer comes first, then every
/// .deb in the lock's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Layer,
    Deb,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub kind: Kind,
    pub key: String,
    pub file: String,
    pub sha256: String,
    pub size: u64,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lock {
    /// The layer first, then the .debs in lock order.
    pub pins: Vec<Pin>,
    /// `sha256:<hex>` NAR hash of the assembled tree, once pinned.
    pub tree: Option<String>,
}

fn is_hex64(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Read the `layer`, `deb` and `tree` rows; every other line is the
/// resolver's input or audit record.
pub fn parse(text: &str) -> Result<Lock, String> {
    let mut pins: Vec<Pin> = Vec::new();
    let mut tree = None;
    for (n, line) in text.lines().enumerate() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let bad = |what: &str| format!("debian-root.lock line {}: {what}", n + 1);
        let (kind, key, file, sha256, size, url) = match fields.as_slice() {
            ["layer", key, file, sha256, size, url] => (Kind::Layer, key, file, sha256, size, url),
            ["deb", key, file, sha256, size, url, _version] => {
                (Kind::Deb, key, file, sha256, size, url)
            }
            ["tree", hash] => {
                let hex = hash
                    .strip_prefix("sha256:")
                    .ok_or_else(|| bad("malformed tree row"))?;
                if !is_hex64(hex) || tree.is_some() {
                    return Err(bad("malformed or repeated tree row"));
                }
                tree = Some(hash.to_string());
                continue;
            }
            ["layer" | "deb" | "tree", ..] => return Err(bad("malformed pin row")),
            _ => continue,
        };
        let basename = !file.is_empty()
            && !file.starts_with('.')
            && !file.contains('/')
            && file.bytes().all(|b| b.is_ascii_graphic());
        let size = size.parse::<u64>().map_err(|_| bad("malformed size"))?;
        if !key.starts_with("debian-root-")
            || !is_hex64(sha256)
            || !basename
            || !url.starts_with("https://")
        {
            return Err(bad("pin row fails validation"));
        }
        if kind == Kind::Layer && !pins.is_empty() {
            return Err(bad("the layer must be the first pin"));
        }
        if kind == Kind::Deb && pins.is_empty() {
            return Err(bad("a .deb before the layer"));
        }
        if pins.iter().any(|pin| pin.key == *key || pin.file == *file) {
            return Err(bad("duplicate key or file"));
        }
        pins.push(Pin {
            kind,
            key: key.to_string(),
            file: file.to_string(),
            sha256: sha256.to_string(),
            size,
            url: url.to_string(),
        });
    }
    if pins.first().map(|pin| pin.kind) != Some(Kind::Layer) {
        return Err("debian-root.lock pins no image layer".into());
    }
    Ok(Lock { pins, tree })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn layer() -> String {
        format!("layer debian-root-layer l.tar.gz {HEX} 1 https://r/l\n")
    }

    fn deb(key: &str, file: &str, sha: &str, url: &str) -> String {
        format!("{}deb {key} {file} {sha} 2 {url} 1.0\n", layer())
    }

    #[test]
    fn the_lock_reads_the_layer_debs_and_tree_in_order() {
        let text = format!(
            "# comment\nimage x\n{}tree sha256:{HEX}\n",
            deb("debian-root-x", "x.deb", HEX, "https://d/x")
        );
        let lock = parse(&text).unwrap();
        assert_eq!(lock.pins.len(), 2);
        assert_eq!(lock.pins[0].kind, Kind::Layer);
        assert_eq!(lock.pins[1].kind, Kind::Deb);
        assert_eq!(lock.pins[1].size, 2);
        assert_eq!(lock.tree, Some(format!("sha256:{HEX}")));
        assert_eq!(parse(&layer()).unwrap().tree, None);
    }

    #[test]
    fn malformed_rows_are_refused() {
        let upper = "A".repeat(64);
        for (text, why) in [
            (deb("x", "x.deb", HEX, "https://d/x"), "unprefixed key"),
            (
                deb("debian-root-x", "a/x.deb", HEX, "https://d/x"),
                "path file",
            ),
            (
                deb("debian-root-x", "x.deb", &upper, "https://d/x"),
                "upper hex",
            ),
            (
                deb("debian-root-x", "x.deb", HEX, "http://d/x"),
                "plain http",
            ),
            (
                deb("debian-root-layer", "x.deb", HEX, "https://d/x"),
                "duplicate key",
            ),
            (
                deb("debian-root-x", "l.tar.gz", HEX, "https://d/x"),
                "duplicate file",
            ),
            (
                format!("{}deb debian-root-x x.deb {HEX} 2 https://d/x\n", layer()),
                "short row",
            ),
            (
                format!("deb debian-root-x x.deb {HEX} 2 https://d/x 1\n"),
                "no layer",
            ),
            (format!("{}{}", layer(), layer()), "second layer"),
            (format!("{}tree {HEX}\n", layer()), "unprefixed tree"),
            (
                format!("{}tree sha256:{HEX}\ntree sha256:{HEX}\n", layer()),
                "two trees",
            ),
        ] {
            assert!(parse(&text).is_err(), "{why}");
        }
    }
}
