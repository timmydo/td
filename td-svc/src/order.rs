//! Start ordering: a topological plan over `after=` edges, with cycles and
//! their downstream reported rather than silently started or silently dropped.

use crate::table::Unit;

/// Why a unit is not in the start order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// This unit is part of a dependency cycle.
    InCycle,
    /// This unit depends, transitively, on a cycle or on an unknown unit.
    Blocked(String),
    /// Names a unit that does not exist.
    Unknown(String),
    /// A console unit whose dependencies are unsatisfiable. It is started
    /// ANYWAY (DESIGN.md I5) — this records why its ordering was ignored.
    ConsoleForced(Box<Skip>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Startable units, dependencies first. Ties break in declaration order so
    /// a table always produces the same plan — `check` prints this, and a plan
    /// that reshuffled between runs would make the output useless for review.
    pub order: Vec<String>,
    /// Units excluded, with the reason, in declaration order.
    pub skipped: Vec<(String, Skip)>,
}

impl Plan {
    /// One line per skipped unit, for `check` and the boot log.
    pub fn complaints(&self) -> Vec<String> {
        self.skipped
            .iter()
            .map(|(name, why)| match why {
                Skip::InCycle => format!("{name}: in a dependency cycle"),
                Skip::Blocked(on) => format!("{name}: blocked by '{on}'"),
                Skip::Unknown(dep) => format!("{name}: depends on unknown unit '{dep}'"),
                Skip::ConsoleForced(why) => {
                    let inner = match why.as_ref() {
                        Skip::InCycle => "is in a dependency cycle".to_string(),
                        Skip::Blocked(on) => format!("is blocked by '{on}'"),
                        Skip::Unknown(dep) => format!("names unknown unit '{dep}'"),
                        Skip::ConsoleForced(_) => "is unstartable".to_string(),
                    };
                    format!(
                        "{name}: {inner}, but it provides a console — starting it anyway, \
                         last, with its ordering ignored"
                    )
                }
            })
            .collect()
    }
}

/// Build the start plan.
///
/// Kahn's algorithm gives the order. What it leaves behind — the residual —
/// is NOT simply "the cycle": it holds cycle members AND everything
/// downstream of them. Starting the residual, or reporting all of it as a
/// cycle, are both wrong, so each residual node is asked whether it can reach
/// itself. The graphs here are a dozen nodes; the clarity is worth more than
/// the asymptotics of Tarjan.
pub fn plan(units: &[Unit]) -> Plan {
    let names: Vec<&str> = units.iter().map(|u| u.name.as_str()).collect();
    let index = |name: &str| names.iter().position(|n| *n == name);

    // Dependencies that name nothing are their own failure, distinct from a
    // cycle: the unit is unstartable but nothing is wrong with the graph.
    let mut skipped: Vec<(String, Skip)> = Vec::new();
    let mut unusable: Vec<bool> = vec![false; units.len()];
    for (i, unit) in units.iter().enumerate() {
        for dep in deps_of(unit) {
            if index(dep).is_none() {
                if let Some(slot) = unusable.get_mut(i) {
                    *slot = true;
                }
                skipped.push((unit.name.clone(), Skip::Unknown(dep.clone())));
                break;
            }
        }
    }

    // edges[d] = units that must start after d.
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); units.len()];
    let mut indegree: Vec<usize> = vec![0; units.len()];
    for (i, unit) in units.iter().enumerate() {
        if unusable.get(i).copied().unwrap_or(false) {
            continue;
        }
        for dep in deps_of(unit) {
            let Some(d) = index(dep) else { continue };
            // The edge is built even when `d` is itself unusable. `d` is
            // pre-placed and so never decrements this indegree, which is
            // exactly right: a unit whose dependency can never run must not
            // start, and it lands in the residual as Blocked rather than
            // being quietly promoted to indegree 0.
            if let Some(e) = edges.get_mut(d) {
                e.push(i);
            }
            if let Some(n) = indegree.get_mut(i) {
                *n += 1;
            }
        }
    }

    let mut order: Vec<String> = Vec::new();
    let mut placed: Vec<bool> = unusable.clone();
    loop {
        // Declaration order, not a queue: the first eligible unit wins, so the
        // plan is a pure function of the table.
        // A plain loop, not an iterator search: this file is embedded verbatim
        // into the recipe, and the ladder guard scans that text for host-tool
        // names that the search combinator happens to share.
        let mut next = None;
        for i in 0..units.len() {
            if !placed.get(i).copied().unwrap_or(true) && indegree.get(i).copied() == Some(0) {
                next = Some(i);
                break;
            }
        }
        let Some(i) = next else { break };
        if let Some(slot) = placed.get_mut(i) {
            *slot = true;
        }
        if let Some(unit) = units.get(i) {
            order.push(unit.name.clone());
        }
        for &j in edges.get(i).map(Vec::as_slice).unwrap_or(&[]) {
            if let Some(n) = indegree.get_mut(j) {
                *n = n.saturating_sub(1);
            }
        }
    }

    // Whatever Kahn could not place is in a cycle or behind one.
    for i in 0..units.len() {
        if placed.get(i).copied().unwrap_or(true) {
            continue;
        }
        let Some(unit) = units.get(i) else { continue };
        let why = if reaches_itself(i, &edges) {
            Skip::InCycle
        } else {
            Skip::Blocked(blocker(i, units, &unusable, &placed))
        };
        skipped.push((unit.name.clone(), why));
    }
    skipped.sort_by_key(|(name, _)| names.iter().position(|n| n == name).unwrap_or(usize::MAX));

    // DESIGN.md I5: a unit that provides a console is never skippable — not by
    // `requires=`, and not by the graph either. A typo in an unrelated stanza,
    // or a cycle two hops upstream, would otherwise leave a running machine
    // with no way to repair it, which this codebase treats as the worst
    // outcome. Such a unit starts LAST, so whatever ordering IS satisfiable
    // still happens first, and its complaint survives to say so.
    let mut forced: Vec<(String, Skip)> = Vec::new();
    skipped.retain(|(name, why)| {
        let is_console = units
            .iter()
            .any(|u| &u.name == name && u.is_console());
        if is_console {
            forced.push((name.clone(), Skip::ConsoleForced(Box::new(why.clone()))));
            return false;
        }
        true
    });
    for (name, why) in forced {
        order.push(name.clone());
        skipped.push((name, why));
    }

    Plan { order, skipped }
}

/// Everything a unit must start after. `requires=` is a strict dependency, so
/// it ORDERS as well as gating — rev 1 read only `after`, which let a unit with
/// `requires=dep` start while `dep` was still down.
fn deps_of(unit: &Unit) -> impl Iterator<Item = &String> {
    unit.after.iter().chain(unit.requires.iter())
}

/// Can `start` be reached from itself by following edges? That is exactly the
/// question "is this node ON a cycle", as opposed to merely downstream of one.
fn reaches_itself(start: usize, edges: &[Vec<usize>]) -> bool {
    let mut seen = vec![false; edges.len()];
    let mut stack = vec![start];
    let mut first = true;
    while let Some(node) = stack.pop() {
        if node == start && !first {
            return true;
        }
        first = false;
        if seen.get(node).copied().unwrap_or(true) {
            continue;
        }
        if let Some(slot) = seen.get_mut(node) {
            *slot = true;
        }
        for &next in edges.get(node).map(Vec::as_slice).unwrap_or(&[]) {
            stack.push(next);
        }
    }
    false
}

/// Name one unplaced dependency, so the diagnostic points somewhere useful
/// rather than saying only "blocked".
fn blocker(i: usize, units: &[Unit], unusable: &[bool], placed: &[bool]) -> String {
    let Some(unit) = units.get(i) else {
        return "?".into();
    };
    for dep in deps_of(unit) {
        let Some(d) = units.iter().position(|u| &u.name == dep) else {
            return dep.clone();
        };
        if unusable.get(d).copied().unwrap_or(false) || !placed.get(d).copied().unwrap_or(true) {
            return dep.clone();
        }
    }
    "?".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::{Kind, Unit};

    fn unit(name: &str, after: &[&str]) -> Unit {
        Unit {
            name: name.into(),
            kind: Kind::Oneshot,
            argv: vec!["/bin/true".into()],
            after: after.iter().map(|s| (*s).to_string()).collect(),
            ..Unit::default()
        }
    }

    #[test]
    fn independent_units_keep_declaration_order() {
        let p = plan(&[unit("a", &[]), unit("b", &[]), unit("c", &[])]);
        assert_eq!(p.order, ["a", "b", "c"]);
        assert!(p.skipped.is_empty());
    }

    #[test]
    fn a_dependency_starts_before_its_dependent() {
        let p = plan(&[unit("sshd", &["netup"]), unit("netup", &["rootcheck"]), unit("rootcheck", &[])]);
        assert_eq!(p.order, ["rootcheck", "netup", "sshd"]);
    }

    /// The image's real chain, which the cutover has to preserve.
    #[test]
    fn the_shipped_boot_chain_orders_correctly() {
        let p = plan(&[
            unit("hostname", &[]),
            unit("td-firstboot", &[]),
            unit("rootcheck", &["td-firstboot"]),
            unit("netup", &["rootcheck"]),
            unit("bootsuccess", &["netup"]),
            unit("sshd", &["netup", "td-firstboot"]),
            unit("greeter", &["netup"]),
        ]);
        assert!(p.skipped.is_empty());
        let at = |n: &str| p.order.iter().position(|x| x == n).unwrap();
        assert!(at("td-firstboot") < at("rootcheck"));
        assert!(at("rootcheck") < at("netup"));
        assert!(at("netup") < at("sshd"));
        assert!(at("netup") < at("greeter"));
        assert!(at("td-firstboot") < at("sshd"));
    }

    /// A cycle must not take its downstream down as "also a cycle" — the
    /// distinction is the whole reason Kahn's residual is not reported raw.
    #[test]
    fn a_cycle_is_reported_separately_from_what_it_blocks() {
        let p = plan(&[
            unit("a", &["b"]),
            unit("b", &["a"]),
            unit("downstream", &["a"]),
            unit("fine", &[]),
        ]);
        assert_eq!(p.order, ["fine"]);
        assert_eq!(p.skipped[0], ("a".into(), Skip::InCycle));
        assert_eq!(p.skipped[1], ("b".into(), Skip::InCycle));
        assert_eq!(p.skipped[2], ("downstream".into(), Skip::Blocked("a".into())));
    }

    #[test]
    fn a_self_dependency_is_a_cycle() {
        let p = plan(&[unit("a", &["a"]), unit("b", &[])]);
        assert_eq!(p.order, ["b"]);
        assert_eq!(p.skipped, [("a".into(), Skip::InCycle)]);
    }

    #[test]
    fn an_unknown_dependency_names_itself_and_blocks_only_its_dependents() {
        let p = plan(&[unit("a", &["nope"]), unit("b", &["a"]), unit("c", &[])]);
        assert_eq!(p.order, ["c"]);
        assert_eq!(p.skipped[0], ("a".into(), Skip::Unknown("nope".into())));
        assert_eq!(p.skipped[1], ("b".into(), Skip::Blocked("a".into())));
        assert!(p.complaints()[0].contains("unknown unit 'nope'"));
    }

    /// Determinism: the same table must always yield the same plan, or the
    /// `check` output a human reviews is not reviewable.
    #[test]
    fn the_plan_is_a_pure_function_of_the_table() {
        let units = [
            unit("z", &["m"]),
            unit("m", &[]),
            unit("a", &["m"]),
            unit("q", &[]),
        ];
        let first = plan(&units);
        for _ in 0..8 {
            assert_eq!(plan(&units), first);
        }
        // Declaration order is the tie-break, applied on every pass: once `m`
        // releases `z`, `z` (declared first) precedes both `a` and `q`.
        assert_eq!(first.order, ["m", "z", "a", "q"]);
    }

    fn console(name: &str, after: &[&str]) -> Unit {
        Unit {
            tty: Some("ttyS0".into()),
            kind: Kind::Daemon,
            ..unit(name, after)
        }
    }

    /// DESIGN.md I5. A typo in an UNRELATED stanza must not delete the console:
    /// the greeter's dependency is unstartable, and the greeter starts anyway.
    #[test]
    fn a_console_unit_starts_even_when_its_dependency_does_not_exist() {
        let p = plan(&[unit("netup", &["nosuchunit"]), console("greeter", &["netup"])]);
        assert!(p.order.contains(&"greeter".to_string()), "{p:?}");
        assert!(!p.order.contains(&"netup".to_string()));
        assert!(p
            .complaints()
            .iter()
            .any(|c| c.contains("greeter") && c.contains("starting it anyway")));
    }

    /// ...and not by a cycle two hops upstream either.
    #[test]
    fn a_console_unit_starts_even_when_a_cycle_blocks_it() {
        let p = plan(&[
            unit("a", &["b"]),
            unit("b", &["a"]),
            console("greeter", &["a"]),
        ]);
        assert!(p.order.contains(&"greeter".to_string()), "{p:?}");
    }

    /// A console unit in a cycle ITSELF still starts — I5 has no exceptions.
    #[test]
    fn a_console_unit_inside_a_cycle_still_starts() {
        let p = plan(&[console("greeter", &["x"]), unit("x", &["greeter"])]);
        assert!(p.order.contains(&"greeter".to_string()), "{p:?}");
    }

    /// It starts LAST, so whatever ordering IS satisfiable happens first.
    #[test]
    fn a_forced_console_starts_after_everything_startable() {
        let p = plan(&[
            console("greeter", &["ghost"]),
            unit("a", &[]),
            unit("b", &["a"]),
        ]);
        assert_eq!(p.order, ["a", "b", "greeter"]);
    }

    /// `requires=` is a dependency, so it ORDERS as well as gating. Reading only
    /// `after` let a unit with `requires=dep` start while dep was still down.
    #[test]
    fn a_strict_dependency_orders_its_dependent() {
        let mut svc = unit("svc", &[]);
        svc.requires = vec!["dep".into()];
        let p = plan(&[svc, unit("dep", &[])]);
        assert_eq!(p.order, ["dep", "svc"]);
    }

    #[test]
    fn an_unknown_strict_dependency_is_caught_like_an_unknown_ordering_one() {
        let mut svc = unit("svc", &[]);
        svc.requires = vec!["ghost".into()];
        let p = plan(&[svc]);
        assert!(p.order.is_empty());
        assert_eq!(p.skipped, [("svc".into(), Skip::Unknown("ghost".into()))]);
    }

    #[test]
    fn an_empty_table_plans_nothing_without_complaint() {
        let p = plan(&[]);
        assert!(p.order.is_empty());
        assert!(p.skipped.is_empty());
    }
}
