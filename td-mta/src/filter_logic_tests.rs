#![allow(clippy::unwrap_used)]
use super::*;
#[test]
fn every_small_flat_truth_table_and_empty_identity_is_exact() {
    for length in 0..=8 {
        for bits in 0..1usize << length {
            for operator in [Operator::And, Operator::Or, Operator::Not] {
                let mut value = Evaluation::new(2, 9).unwrap();
                value.begin(operator).unwrap();
                for index in 0..length {
                    value.condition(bits & (1 << index) != 0).unwrap();
                }
                value.end().unwrap();
                let expected = match operator {
                    Operator::And => bits == (1 << length) - 1,
                    Operator::Or => bits != 0,
                    Operator::Not => bits == 0,
                };
                assert_eq!(value.finish().unwrap(), expected);
            }
        }
    }
    for bit in [false, true] {
        let mut value = Evaluation::new(1, 1).unwrap();
        value.condition(bit).unwrap();
        assert_eq!(value.finish().unwrap(), bit);
    }
}
#[test]
fn nested_mixed_operators_match_an_independent_boolean_expression() {
    for bits in 0..16u8 {
        let a = bits & 1 != 0;
        let b = bits & 2 != 0;
        let c = bits & 4 != 0;
        let d = bits & 8 != 0;
        // AND(OR(a,b), NOT(c,d), AND([]), NOT(OR([]))).
        let mut value = Evaluation::new(3, 10).unwrap();
        value.begin(Operator::And).unwrap();
        value.begin(Operator::Or).unwrap();
        value.condition(a).unwrap();
        value.condition(b).unwrap();
        value.end().unwrap();
        value.begin(Operator::Not).unwrap();
        value.condition(c).unwrap();
        value.condition(d).unwrap();
        value.end().unwrap();
        value.begin(Operator::And).unwrap();
        value.end().unwrap();
        value.begin(Operator::Not).unwrap();
        value.begin(Operator::Or).unwrap();
        value.end().unwrap();
        value.end().unwrap();
        value.end().unwrap();
        assert_eq!(value.finish().unwrap(), (a || b) && !(c || d));
    }
}
#[test]
fn depth_and_node_limits_include_conditions_and_retire_partial_results() {
    for maximum in 1..=MAX_DEPTH {
        let mut value = Evaluation::new(maximum, MAX_DEPTH + 1).unwrap();
        for _ in 1..maximum {
            value.begin(Operator::Not).unwrap();
        }
        value.condition(true).unwrap();
        for _ in 1..maximum {
            value.end().unwrap();
        }
        assert_eq!(value.finish().unwrap(), !maximum.is_multiple_of(2));
        let mut value = Evaluation::new(maximum, MAX_DEPTH + 1).unwrap();
        for _ in 0..maximum {
            value.begin(Operator::Not).unwrap();
        }
        assert_eq!(value.condition(true), Err(Error::DepthLimit));
        assert_eq!(value.end(), Err(Error::Failed));
        assert_eq!(value.finish(), Err(Error::Failed));
    }
    for operator in [Operator::And, Operator::Or, Operator::Not] {
        let mut value = Evaluation::new(2, 2).unwrap();
        value.begin(operator).unwrap();
        value.condition(operator != Operator::And).unwrap();
        assert_eq!(value.condition(false), Err(Error::NodeLimit));
        assert_eq!(value.begin(Operator::Or), Err(Error::Failed));
        assert_eq!(value.finish(), Err(Error::Failed));
    }
    let mut value = Evaluation::new(MAX_DEPTH, MAX_DEPTH).unwrap();
    for _ in 0..MAX_DEPTH {
        value.begin(Operator::Not).unwrap();
    }
    for _ in 0..MAX_DEPTH {
        value.end().unwrap();
    }
    assert!(!value.finish().unwrap());
    assert!(std::mem::size_of::<Evaluation>() <= 256);
}
#[test]
fn malformed_event_streams_never_expose_a_partial_truth_value() {
    assert!(matches!(Evaluation::new(0, 1), Err(Error::Invalid)));
    assert!(matches!(
        Evaluation::new(MAX_DEPTH + 1, 1),
        Err(Error::Invalid)
    ));
    assert!(matches!(Evaluation::new(1, 0), Err(Error::Invalid)));
    assert_eq!(Evaluation::new(1, 1).unwrap().finish(), Err(Error::Invalid));
    let mut value = Evaluation::new(1, 1).unwrap();
    value.begin(Operator::And).unwrap();
    assert_eq!(value.finish(), Err(Error::Invalid));
    let mut value = Evaluation::new(1, 1).unwrap();
    assert_eq!(value.end(), Err(Error::Invalid));
    assert_eq!(value.condition(true), Err(Error::Failed));
    for is_operator in [false, true] {
        let mut value = Evaluation::new(2, 3).unwrap();
        value.condition(true).unwrap();
        let error = if is_operator {
            value.begin(Operator::And)
        } else {
            value.condition(false)
        };
        assert_eq!(error, Err(Error::Invalid));
        assert_eq!(value.finish(), Err(Error::Failed));
    }
    for (text, expected) in [
        ("AND", Some(Operator::And)),
        ("OR", Some(Operator::Or)),
        ("NOT", Some(Operator::Not)),
        ("and", None),
        ("XOR", None),
        ("", None),
    ] {
        assert_eq!(Operator::parse(text), expected);
    }
}

#[test]
fn operator_limit_failures_and_completed_roots_are_sticky() {
    fn retired(mut value: Evaluation) {
        assert_eq!(value.condition(true), Err(Error::Failed));
        assert_eq!(value.begin(Operator::Or), Err(Error::Failed));
        assert_eq!(value.end(), Err(Error::Failed));
        assert_eq!(value.finish(), Err(Error::Failed));
    }
    for maximum in 1..=MAX_DEPTH {
        let mut value = Evaluation::new(maximum, MAX_DEPTH + 1).unwrap();
        for _ in 0..maximum {
            value.begin(Operator::Not).unwrap();
        }
        assert_eq!(value.begin(Operator::And), Err(Error::DepthLimit));
        retired(value);
    }
    let mut value = Evaluation::new(MAX_DEPTH, MAX_DEPTH - 1).unwrap();
    for _ in 1..MAX_DEPTH {
        value.begin(Operator::And).unwrap();
    }
    assert_eq!(value.begin(Operator::Or), Err(Error::NodeLimit));
    retired(value);
    for tail in 0..3 {
        let mut value = Evaluation::new(2, 3).unwrap();
        value.begin(Operator::And).unwrap();
        value.end().unwrap();
        let result = match tail {
            0 => value.begin(Operator::Or),
            1 => value.condition(true),
            _ => value.end(),
        };
        assert_eq!(result, Err(Error::Invalid));
        retired(value);
    }
}

#[test]
fn small_trees_match_recursive_oracle_at_exact_limits() {
    #[derive(Clone)]
    enum Tree {
        Leaf(bool),
        Op(Operator, Vec<Tree>),
    }
    impl Tree {
        fn expected(&self) -> bool {
            match self {
                Self::Leaf(value) => *value,
                Self::Op(Operator::And, children) => children.iter().all(Self::expected),
                Self::Op(Operator::Or, children) => children.iter().any(Self::expected),
                Self::Op(Operator::Not, children) => !children.iter().any(Self::expected),
            }
        }
        fn depth(&self) -> usize {
            match self {
                Self::Leaf(_) => 1,
                Self::Op(_, children) => 1 + children.iter().map(Self::depth).max().unwrap_or(0),
            }
        }
        fn feed(&self, value: &mut Evaluation) -> Result<(), Error> {
            match self {
                Self::Leaf(bit) => value.condition(*bit),
                Self::Op(operator, children) => {
                    value.begin(*operator)?;
                    for child in children {
                        child.feed(value)?;
                    }
                    value.end()
                }
            }
        }
    }
    fn forests(size: usize, trees: &[Vec<Tree>]) -> Vec<Vec<Tree>> {
        if size == 0 {
            return vec![vec![]];
        }
        let mut result = Vec::new();
        for first in 1..=size {
            for tree in trees.get(first).unwrap() {
                for mut tail in forests(size - first, trees) {
                    tail.insert(0, tree.clone());
                    result.push(tail);
                }
            }
        }
        result
    }
    let mut trees = vec![Vec::new()];
    for size in 1..=5 {
        let mut level = Vec::new();
        if size == 1 {
            level.extend([Tree::Leaf(false), Tree::Leaf(true)]);
        }
        for children in forests(size - 1, &trees) {
            for operator in [Operator::And, Operator::Or, Operator::Not] {
                level.push(Tree::Op(operator, children.clone()));
            }
        }
        for tree in &level {
            let depth = tree.depth();
            let mut value = Evaluation::new(depth, size).unwrap();
            tree.feed(&mut value).unwrap();
            assert_eq!(value.finish().unwrap(), tree.expected());
            if depth > 1 {
                let mut value = Evaluation::new(depth - 1, size).unwrap();
                assert_eq!(tree.feed(&mut value), Err(Error::DepthLimit));
                assert_eq!(value.finish(), Err(Error::Failed));
            }
            if size > 1 {
                let mut value = Evaluation::new(depth, size - 1).unwrap();
                assert_eq!(tree.feed(&mut value), Err(Error::NodeLimit));
                assert_eq!(value.finish(), Err(Error::Failed));
            }
        }
        trees.push(level);
    }
}
