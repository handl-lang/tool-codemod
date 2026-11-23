//! Compiling [`CodemodRule`]s into `hnl-tool-transform-rewrite`
//! dispatch data.
//!
//! Each file-scope rule with a `rewrite:` becomes one or more
//! [`TransformDef`]s in a synthetic `"codemod"` pass:
//!
//! - `match:` decodes via [`CodemodMatch`] — a kinded match yields one
//!   def; a bare `TransformCondition` ([`CodemodMatch::Any`]) or an
//!   absent `match:` expands to one def **per node kind the `Language`
//!   declares** (`lang.ast` shape `node_kind`s, deduplicated), so the
//!   engine's `kind → rules` dispatch stays the only matcher.
//! - `rewrite: "handl:<mod>.<fn>"` / `"native:<name>"` / bare name →
//!   [`TransformRule::Native`] — resolved at run time by the wired
//!   [`NativeRewrite`](hnl_tool_transform::native::NativeRewrite)
//!   registry (`native:` is stripped; `handl:` passes through
//!   verbatim for the host's `.hnl` resolver).
//! - `rewrite: { delete: true }` → the internal
//!   [`DELETE_NATIVE`](crate::native::DELETE_NATIVE) native, which
//!   produces a tombstone node the engine strips in a post-pass (the
//!   transform vocabulary has no delete primitive — deletion is a
//!   codemod-level concern, see [`crate::compact`]).
//!
//! Rules with **no** `rewrite:` are observe-only (the lint-as-codemod
//! path): they never enter the rewrite pass; their matches are
//! collected by [`observe_matches`] — a plain arena walk evaluating the
//! same `TransformCondition` vocabulary via
//! [`matches_condition`](hnl_tool_transform::condition::matches_condition).
//!
//! `priority` carries the rule's declaration index so fire order is
//! spec order (the `RuleSource`/`Dispatch` sort is `priority` then
//! `id`).

use std::collections::BTreeSet;

use handl_language::{
    Ast, AstNode, AstRef, Language, TransformCondition, TransformDef, TransformRule,
};
use hnl_tool_transform::condition::matches_condition;
use hnl_tool_transform::source::RuleSource;

use crate::match_spec::CodemodMatch;
use crate::native::DELETE_NATIVE;
use crate::spec::{CodemodRewrite, CodemodRule, CodemodSpec, RuleScope};

/// The synthetic pass name codemod rules compile into.
pub const CODEMOD_PASS: &str = "codemod";

/// A codemod rule after `match:` decoding — the spec fields the engine
/// needs, plus its compiled dispatch defs.
#[derive(Debug)]
pub(crate) struct CompiledRule {
    /// The spec rule (id, scope, requires, on_engine_error, rewrite).
    pub rule: CodemodRule,
    /// Decoded `match:` — `None` = unconditional (every kind).
    pub mat: Option<CodemodMatch>,
    /// `TransformDef`s for the rewrite pass (empty for observe rules).
    pub defs: Vec<TransformDef>,
    /// Human-readable rewrite spec for edit records (`"handl:…"`,
    /// `"native:…"`, `"delete"`); `None` for observe rules.
    pub rewrite_desc: Option<String>,
}

/// Every node kind `lang` declares an AST shape for — the expansion
/// set for kind-less matches. Deduplicated and sorted for
/// determinism.
pub(crate) fn declared_kinds(lang: &Language) -> Vec<String> {
    lang.ast
        .iter()
        .map(|s| s.node_kind.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Compile one spec rule against `lang`.
///
/// Returns `Err` when `match:` fails to decode — the caller turns that
/// into a diagnostic and skips the rule (an honest decline, never a
/// silently-widened match).
pub(crate) fn compile_rule(
    rule: &CodemodRule,
    index: usize,
    lang: &Language,
) -> Result<CompiledRule, String> {
    let mat = CodemodMatch::decode(rule.condition.as_ref())
        .map_err(|e| format!("rule '{}': {e}", rule.id))?;

    let rewrite_desc = rule.rewrite.as_ref().map(|r| match r {
        CodemodRewrite::Named(s) => s
            .strip_prefix("native:")
            .map_or_else(|| s.clone(), |n| format!("native:{n}")),
        CodemodRewrite::Delete { .. } => "delete".to_string(),
    });

    let mut defs = Vec::new();
    if let Some(rewrite) = &rule.rewrite {
        let transform_rule = match rewrite {
            CodemodRewrite::Named(name) => {
                // `native:<name>` names a registered native; strip the
                // tag so the registry sees the bare name. `handl:…`
                // and bare names pass through verbatim.
                let native = name.strip_prefix("native:").unwrap_or(name);
                TransformRule::Native(native.to_string())
            }
            CodemodRewrite::Delete { delete: true } => {
                TransformRule::Native(DELETE_NATIVE.to_string())
            }
            CodemodRewrite::Delete { delete: false } => unreachable!("validated"),
        };
        let (kinds, when): (Vec<String>, Option<TransformCondition>) = match &mat {
            Some(CodemodMatch::Node { kind, when }) => (vec![kind.clone()], when.clone()),
            Some(CodemodMatch::Any(cond)) => (declared_kinds(lang), Some(cond.clone())),
            None => (declared_kinds(lang), None),
        };
        for kind in kinds {
            let mut def =
                TransformDef::new(rule.id.clone(), CODEMOD_PASS, kind, transform_rule.clone())
                    .with_priority(index as i32);
            if let Some(cond) = &when {
                def = def.with_when(cond.clone());
            }
            defs.push(def);
        }
    }

    Ok(CompiledRule {
        rule: rule.clone(),
        mat,
        defs,
        rewrite_desc,
    })
}

/// Compile the spec's **file-scope** rules for `lang`, in declaration
/// order. Observe rules (no `rewrite:`) produce no defs but are still
/// returned for [`observe_matches`]. Invalid `match:` values yield a
/// diagnostic via the returned `Vec`'s companion — see
/// [`compile_rules`].
pub(crate) fn compile_rules(
    spec: &CodemodSpec,
    lang: &Language,
) -> (Vec<CompiledRule>, Vec<(String, String)>) {
    let mut rules = Vec::new();
    let mut errors = Vec::new();
    for (i, rule) in spec.rules.iter().enumerate() {
        if rule.scope != RuleScope::File {
            continue;
        }
        match compile_rule(rule, i, lang) {
            Ok(c) => rules.push(c),
            Err(msg) => errors.push((rule.id.clone(), msg)),
        }
    }
    (rules, errors)
}

/// A [`RuleSource`] serving a fixed set of compiled defs — one per
/// artifact (rules gated out by `requires:` simply aren't in the set).
pub(crate) struct CodemodRuleSource {
    defs: Vec<TransformDef>,
}

impl CodemodRuleSource {
    pub(crate) fn new(defs: Vec<TransformDef>) -> Self {
        Self { defs }
    }
}

impl RuleSource for CodemodRuleSource {
    fn rules_for(&self, _lang: &Language, pass: &str) -> Vec<TransformDef> {
        if pass == CODEMOD_PASS {
            self.defs.clone()
        } else {
            Vec::new()
        }
    }
}

/// Input-arena indices reachable from `root` through `children`
/// arrays — same convention as the rewrite engine's `find_matches`.
pub(crate) fn reachable_from(ast: &Ast, root: AstRef) -> BTreeSet<usize> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![root.index()];
    while let Some(idx) = stack.pop() {
        if !seen.insert(idx) {
            continue;
        }
        if let Some(node) = ast.get(AstRef::new(idx))
            && let Some(children) = node.payload.get("children").and_then(|c| c.as_array())
        {
            for v in children {
                if let Some(c) = v.as_u64() {
                    stack.push(c as usize);
                }
            }
        }
    }
    seen
}

/// One observe-rule match: `(rule declaration index, node)`.
pub(crate) struct ObserveMatch {
    /// Index into the compiled rule list (spec order preserved).
    pub rule_idx: usize,
    /// The matched input node.
    pub at: AstRef,
}

/// Collect observe-rule (no `rewrite:`) matches over one statement
/// arena — every `(rule, node)` pair where the rule's `match:` holds.
/// Nodes are visited in ascending arena order; rules in spec order, so
/// output is deterministic.
pub(crate) fn observe_matches(
    lang: &Language,
    ast: &Ast,
    root: AstRef,
    rules: &[&CompiledRule],
) -> Vec<ObserveMatch> {
    let mut out = Vec::new();
    for idx in reachable_from(ast, root) {
        let Some(node) = ast.get(AstRef::new(idx)) else {
            continue;
        };
        for (ri, cr) in rules.iter().enumerate() {
            if cr.rule.rewrite.is_some() {
                continue;
            }
            if observe_fires(lang, ast, node, &cr.mat) {
                out.push(ObserveMatch {
                    rule_idx: ri,
                    at: AstRef::new(idx),
                });
            }
        }
    }
    out
}

/// Whether an observe rule's `match:` holds on `node`.
fn observe_fires(lang: &Language, ast: &Ast, node: &AstNode, mat: &Option<CodemodMatch>) -> bool {
    match mat {
        None => true,
        Some(CodemodMatch::Node { kind, when }) => {
            if node.kind != *kind {
                return false;
            }
            match when {
                Some(cond) => matches_condition(lang, ast, node, cond),
                None => true,
            }
        }
        Some(CodemodMatch::Any(cond)) => matches_condition(lang, ast, node, cond),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{
        CodemodRewrite, CodemodRule, CodemodSpec, CodemodTarget, EngineErrorPolicy, UnparseMode,
    };
    use serde_json::{Value, json};

    fn spec_with(rules: Vec<CodemodRule>) -> CodemodSpec {
        CodemodSpec {
            name: "t".into(),
            version: 1,
            targets: vec![CodemodTarget {
                language: "handl".into(),
                glob: "**/*.hnl".into(),
                unparse: UnparseMode::None,
            }],
            rules,
        }
    }

    fn rule(id: &str, condition: Option<Value>, rewrite: Option<CodemodRewrite>) -> CodemodRule {
        CodemodRule {
            id: id.into(),
            scope: RuleScope::File,
            requires: vec![],
            on_engine_error: EngineErrorPolicy::Skip,
            condition,
            rewrite,
        }
    }

    #[test]
    fn kinded_rule_compiles_one_def() {
        let lang = Language::new();
        let spec = spec_with(vec![rule(
            "r1",
            Some(json!({"kind": "call"})),
            Some(CodemodRewrite::Named("handl:m.f".into())),
        )]);
        let (rules, errs) = compile_rules(&spec, &lang);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].defs.len(), 1);
        let def = &rules[0].defs[0];
        assert_eq!(def.node_kind, "call");
        assert_eq!(def.id, "r1");
        assert_eq!(def.pass, CODEMOD_PASS);
        assert!(matches!(def.rule, TransformRule::Native(ref n) if n == "handl:m.f"));
    }

    #[test]
    fn native_prefix_is_stripped() {
        let lang = Language::new();
        let spec = spec_with(vec![rule(
            "r1",
            Some(json!("var")),
            Some(CodemodRewrite::Named("native:swap".into())),
        )]);
        let (rules, _) = compile_rules(&spec, &lang);
        assert!(matches!(&rules[0].defs[0].rule, TransformRule::Native(n) if n == "swap"));
    }

    #[test]
    fn delete_compiles_to_delete_native() {
        let lang = Language::new();
        let spec = spec_with(vec![rule(
            "r1",
            Some(json!("call")),
            Some(CodemodRewrite::Delete { delete: true }),
        )]);
        let (rules, _) = compile_rules(&spec, &lang);
        assert!(matches!(&rules[0].defs[0].rule, TransformRule::Native(n) if n == DELETE_NATIVE));
        assert_eq!(rules[0].rewrite_desc.as_deref(), Some("delete"));
    }

    #[test]
    fn any_condition_expands_over_declared_kinds() {
        let mut lang = Language::new();
        lang.merge_ast_fragment(handl_language::AstShapeDef::new("ast.a", "a"))
            .unwrap();
        lang.merge_ast_fragment(handl_language::AstShapeDef::new("ast.b", "b"))
            .unwrap();
        let spec = spec_with(vec![rule(
            "r1",
            Some(json!({"ChildCount": 0})),
            Some(CodemodRewrite::Named("n".into())),
        )]);
        let (rules, errs) = compile_rules(&spec, &lang);
        assert!(errs.is_empty());
        assert_eq!(rules[0].defs.len(), 2);
        let kinds: Vec<&str> = rules[0].defs.iter().map(|d| d.node_kind.as_str()).collect();
        assert_eq!(kinds, ["a", "b"]);
    }

    #[test]
    fn observe_rule_has_no_defs() {
        let lang = Language::new();
        let spec = spec_with(vec![rule("obs", Some(json!("call")), None)]);
        let (rules, errs) = compile_rules(&spec, &lang);
        assert!(errs.is_empty());
        assert_eq!(rules[0].defs.len(), 0);
        assert_eq!(rules[0].rewrite_desc, None);
    }

    #[test]
    fn bad_match_is_an_error_not_a_widened_rule() {
        let lang = Language::new();
        let spec = spec_with(vec![rule(
            "r1",
            Some(json!({"kind": 3})),
            Some(CodemodRewrite::Named("n".into())),
        )]);
        let (rules, errs) = compile_rules(&spec, &lang);
        assert!(rules.is_empty());
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].0, "r1");
    }
}
