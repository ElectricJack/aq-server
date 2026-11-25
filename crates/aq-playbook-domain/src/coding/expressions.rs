use super::*;

pub(super) fn lit(v: Value) -> Expr {
    Expr::literal(v)
}
pub(super) fn r(root: &str, path: &[&str]) -> Expr {
    Expr::Ref {
        root: root.into(),
        path: path.iter().map(|s| (*s).into()).collect(),
    }
}
pub(super) fn fields(items: &[(&str, Expr)]) -> Expr {
    Expr::Object {
        fields: items.iter().cloned().map(|(k, v)| (k.into(), v)).collect(),
    }
}
pub(super) fn eq(left: Expr, right: &str) -> Expr {
    Expr::Eq {
        left: Box::new(left),
        right: Box::new(lit(json!(right))),
    }
}
pub(super) fn edges(op: Operation, edges: &[(&str, &str)]) -> Step {
    Step {
        op,
        edges: edges
            .iter()
            .map(|(a, b)| ((*a).into(), (*b).into()))
            .collect(),
        on_error: Some("failure".into()),
    }
}
pub(super) struct DecisionEdges<'a> {
    pub on_true: &'a str,
    pub on_false: &'a str,
}
pub(super) fn decide(predicate: Expr, targets: DecisionEdges<'_>) -> Step {
    edges(
        Operation::Decide { predicate },
        &[
            ("true", targets.on_true),
            ("false", targets.on_false),
            ("unknown", "failure"),
        ],
    )
}
pub(super) fn ret(outcome: &str, output: Expr) -> Step {
    Step {
        op: Operation::Return {
            outcome: outcome.into(),
            output,
        },
        edges: BTreeMap::new(),
        on_error: None,
    }
}
/// Both immediate command results and event-completed results expose a
/// reference and subject. A completion envelope nests them under `output`.
pub(super) enum BoundResult<'a> {
    Direct(&'a str),
    Completion(&'a str),
}
impl BoundResult<'_> {
    pub(super) fn projection(&self) -> Expr {
        let at = |field| match self {
            Self::Direct(binding) => r(binding, &[field]),
            Self::Completion(binding) => r(binding, &["output", field]),
        };
        fields(&[("reference", at("reference")), ("subject", at("subject"))])
    }
}
pub(super) fn call(dependency: &str, input: Expr, bind: &str, outcomes: &[(&str, &str)]) -> Step {
    edges(
        Operation::Call {
            dependency: dependency.into(),
            entry: "main".into(),
            input,
            mode: CallMode::Wait,
            bind: bind.into(),
        },
        outcomes,
    )
}
