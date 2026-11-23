//! Integration tests for `CodemodEngine` (T-081–T-084): edit
//! collection on real `.hnl` sources, preserve/full/none materialize
//! modes, scope diagnostics, `requires:` context caching, and
//! deterministic sorted output.
//!
//! The canonical `handl` `Language` comes from
//! `handl-kit-data-prelude::assemble()` (dev-dep); the store is
//! `engine-codegen`'s `MemoryStore`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use handl_kit_data_prelude::assemble;
use handl_language::{AstNode, AstRef, Language};
use hnl_engine_codegen::{ArtifactRef, ArtifactStore, MemoryStore};
use hnl_engine_codemod::{
    CodemodEngine, CodemodError, CodemodRewrite, CodemodRule, CodemodSpec, CodemodTarget,
    DiagnosticKind, EngineContextError, EngineContextProvider, EngineContextReport,
    EngineContextRequest, EngineErrorPolicy, RuleScope, StaticLanguageResolver, TargetArtifacts,
    UnparseMode,
};
use hnl_engine_transform::native::{NativeRewrite, RewriteCtx};
use serde_json::{Value, json};

fn lang() -> Arc<Language> {
    Arc::new(assemble().unwrap().lang().clone())
}

fn resolver() -> StaticLanguageResolver {
    StaticLanguageResolver::new().with_language("handl", lang())
}

fn spec(rules: Vec<CodemodRule>, unparse: UnparseMode) -> CodemodSpec {
    CodemodSpec {
        name: "t".into(),
        version: 1,
        targets: vec![CodemodTarget {
            language: "handl".into(),
            glob: "**/*.hnl".into(),
            unparse,
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

fn store_with(entries: &[(&str, &str)]) -> MemoryStore {
    let store = MemoryStore::new();
    for (name, src) in entries {
        store
            .write(&ArtifactRef::new(*name), src.as_bytes())
            .unwrap();
    }
    store
}

fn targets(refs: &[&str]) -> Vec<TargetArtifacts> {
    vec![TargetArtifacts {
        target: 0,
        artifacts: refs.iter().map(|r| ArtifactRef::new(*r)).collect(),
    }]
}

/// A native that rewrites every `value` (literal) node to payload `99`.
struct ValueTo99;

impl NativeRewrite for ValueTo99 {
    fn rewrite(
        &self,
        name: &str,
        ctx: &mut RewriteCtx<'_>,
    ) -> Result<AstRef, handl_language::LangError> {
        assert_eq!(name, "test.value99");
        let idx = ctx
            .out()
            .push(AstNode::new("value").with_payload(json!(99)))
            .index();
        Ok(AstRef::new(idx))
    }
    fn provides(&self, name: &str) -> bool {
        name == "test.value99"
    }
}

fn engine<'s>(store: &'s MemoryStore) -> CodemodEngine<'s, MemoryStore> {
    CodemodEngine::new(store)
        .with_language_resolver(resolver())
        .with_rewrite_resolver(Arc::new(ValueTo99))
}

fn value99_rule() -> CodemodRule {
    rule(
        "v99",
        Some(json!("value")),
        Some(CodemodRewrite::Named("native:test.value99".into())),
    )
}

fn change_text(plan: &hnl_engine_codemod::CodemodPlan, artifact: &str) -> String {
    let change = plan
        .changes
        .iter()
        .find(|c| c.artifact().as_str() == artifact)
        .unwrap_or_else(|| panic!("no change for {artifact}"));
    String::from_utf8(change.content().unwrap().to_vec()).unwrap()
}

// --- T-081: edit collection -------------------------------------------

#[test]
fn preserve_rewrites_literals_and_keeps_the_rest() {
    let store = store_with(&[("a.hnl", "A := 1\nB := 2\n")]);
    let spec = spec(vec![value99_rule()], UnparseMode::Preserve);
    let plan = engine(&store).plan(&spec, &targets(&["a.hnl"])).unwrap();

    assert_eq!(plan.stats.edits, 2, "one edit per `value` node");
    assert!(
        plan.edits
            .iter()
            .all(|e| e.rule_id == "v99" && e.node_kind == "value" && !e.deleted)
    );
    assert_eq!(plan.changes.len(), 1);
    let text = change_text(&plan, "a.hnl");
    assert_eq!(text, "A := 99\nB := 99\n", "got {text:?}");
}

#[test]
fn edit_records_carry_span_before_after() {
    let store = store_with(&[("a.hnl", "A := 1\n")]);
    let spec = spec(vec![value99_rule()], UnparseMode::None);
    let plan = engine(&store).plan(&spec, &targets(&["a.hnl"])).unwrap();

    assert_eq!(plan.edits.len(), 1);
    let e = &plan.edits[0];
    assert_eq!(e.artifact.as_str(), "a.hnl");
    assert_eq!(e.before.as_deref(), Some("1"));
    assert_eq!(e.after.as_deref(), Some("99"));
    assert_eq!(e.rewrite.as_deref(), Some("native:test.value99"));
    assert!(e.span.is_some());
}

// --- T-082: unparse modes ----------------------------------------------

#[test]
fn unparse_none_is_plan_only() {
    let store = store_with(&[("a.hnl", "A := 1\n")]);
    let spec = spec(vec![value99_rule()], UnparseMode::None);
    let plan = engine(&store).plan(&spec, &targets(&["a.hnl"])).unwrap();

    assert_eq!(plan.edits.len(), 1, "edits still recorded");
    assert!(plan.changes.is_empty(), "none emits no Change");
    assert!(plan.is_empty());
}

#[test]
fn unparse_full_reemits_the_artifact() {
    let store = store_with(&[("a.hnl", "A:=1\n")]);
    let spec = spec(vec![value99_rule()], UnparseMode::Full);
    let plan = engine(&store).plan(&spec, &targets(&["a.hnl"])).unwrap();

    assert_eq!(plan.changes.len(), 1);
    let text = change_text(&plan, "a.hnl");
    assert!(text.contains("99"), "got {text:?}");
}

#[test]
fn observe_rule_collects_edits_without_changes() {
    let store = store_with(&[("a.hnl", "A := 1\n")]);
    let spec = spec(
        vec![rule("obs", Some(json!("assign")), None)],
        UnparseMode::Preserve,
    );
    let plan = engine(&store).plan(&spec, &targets(&["a.hnl"])).unwrap();

    assert_eq!(plan.edits.len(), 1);
    let e = &plan.edits[0];
    assert_eq!(e.rule_id, "obs");
    assert_eq!(e.node_kind, "assign");
    assert_eq!(e.rewrite, None);
    assert_eq!(e.after, None);
    assert!(plan.changes.is_empty(), "observe rules never write");
}

#[test]
fn delete_rewrite_removes_the_matched_statement() {
    let store = store_with(&[("a.hnl", "A := 1\nB := 2\n")]);
    let spec = spec(
        vec![rule(
            "del",
            Some(json!("assign")),
            Some(CodemodRewrite::Delete { delete: true }),
        )],
        UnparseMode::Preserve,
    );
    let plan = engine(&store).plan(&spec, &targets(&["a.hnl"])).unwrap();

    assert_eq!(plan.edits.len(), 2);
    assert!(plan.edits.iter().all(|e| e.deleted && e.after.is_none()));
    assert_eq!(plan.changes.len(), 1);
    let text = change_text(&plan, "a.hnl");
    assert!(!text.contains(":="), "statements deleted, got {text:?}");
}

// --- T-083: requires / engine contexts ----------------------------------

/// A `requires:` provider that counts `provide` calls and can be told
/// to fail. The counter is shared so the test can observe it after the
/// provider is moved into the engine.
struct CountingProvider {
    calls: Arc<AtomicUsize>,
    fail: bool,
}

impl CountingProvider {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                calls: calls.clone(),
                fail: false,
            },
            calls,
        )
    }
    fn failing() -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                calls: calls.clone(),
                fail: true,
            },
            calls,
        )
    }
}

impl EngineContextProvider for CountingProvider {
    fn engine_name(&self) -> &str {
        "typecheck"
    }
    fn engine_version(&self) -> &str {
        "1"
    }
    fn provide(
        &self,
        _req: &EngineContextRequest<'_>,
    ) -> Result<EngineContextReport, EngineContextError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(EngineContextError("boom".into()));
        }
        Ok(EngineContextReport {
            engine: "typecheck".into(),
            version: "1".into(),
            report: json!({"ok": true}),
        })
    }
}

fn requires_rule(id: &str, engine: &str, policy: EngineErrorPolicy) -> CodemodRule {
    let mut r = rule(
        id,
        Some(json!("value")),
        Some(CodemodRewrite::Named("native:test.value99".into())),
    );
    r.requires = vec![engine.to_string()];
    r.on_engine_error = policy;
    r
}

#[test]
fn requires_context_is_cached_per_artifact_hash() {
    // Two artifacts, identical bytes → identical hash → one provider call.
    let (provider, calls) = CountingProvider::new();
    let store = store_with(&[("a.hnl", "A := 1\n"), ("b.hnl", "A := 1\n")]);
    let spec = spec(
        vec![requires_rule("r", "typecheck", EngineErrorPolicy::Skip)],
        UnparseMode::None,
    );
    let eng = CodemodEngine::new(&store)
        .with_language_resolver(resolver())
        .with_rewrite_resolver(Arc::new(ValueTo99))
        .with_engine_context(provider);
    let plan = eng.plan(&spec, &targets(&["a.hnl", "b.hnl"])).unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1, "hash-keyed cache dedups");
    assert_eq!(plan.stats.engine_context_calls, 1);
    assert_eq!(plan.edits.len(), 2);
    assert!(
        plan.diagnostics
            .iter()
            .all(|d| d.kind != DiagnosticKind::EngineUnavailable)
    );
}

#[test]
fn missing_provider_skips_rule_under_skip_policy() {
    let store = store_with(&[("a.hnl", "A := 1\n")]);
    let spec = spec(
        vec![requires_rule("r", "typecheck", EngineErrorPolicy::Skip)],
        UnparseMode::None,
    );
    // No provider registered.
    let plan = engine(&store).plan(&spec, &targets(&["a.hnl"])).unwrap();

    assert!(plan.edits.is_empty());
    assert!(
        plan.diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::EngineUnavailable
                && d.engine.as_deref() == Some("typecheck")
                && d.rule_id.as_deref() == Some("r"))
    );
    assert_eq!(plan.stats.rules_skipped_engine, 1);
}

#[test]
fn failing_provider_aborts_under_fail_policy() {
    let (provider, _calls) = CountingProvider::failing();
    let store = store_with(&[("a.hnl", "A := 1\n")]);
    let spec = spec(
        vec![requires_rule("r", "typecheck", EngineErrorPolicy::Fail)],
        UnparseMode::None,
    );
    let eng = CodemodEngine::new(&store)
        .with_language_resolver(resolver())
        .with_rewrite_resolver(Arc::new(ValueTo99))
        .with_engine_context(provider);
    let err = eng.plan(&spec, &targets(&["a.hnl"])).unwrap_err();
    match err {
        CodemodError::Engine {
            rule_id, engine, ..
        } => {
            assert_eq!(rule_id, "r");
            assert_eq!(engine, "typecheck");
        }
        other => panic!("expected Engine error, got {other}"),
    }
}

// --- T-084: scheduling / determinism -------------------------------------

#[test]
fn program_scope_rules_are_reported_not_run() {
    let store = store_with(&[("a.hnl", "A := 1\n")]);
    let mut r = rule("prog", Some(json!("assign")), None);
    r.scope = RuleScope::Program;
    let spec = spec(vec![r], UnparseMode::None);
    let plan = engine(&store).plan(&spec, &targets(&["a.hnl"])).unwrap();

    assert!(
        plan.diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::ProgramScopeUnsupported
                && d.rule_id.as_deref() == Some("prog"))
    );
    assert!(
        plan.edits.is_empty(),
        "program rules produce no file edits in v1"
    );
}

#[test]
fn changes_are_sorted_by_artifact_ref() {
    let store = store_with(&[("b.hnl", "B := 1\n"), ("a.hnl", "A := 1\n")]);
    let spec = spec(vec![value99_rule()], UnparseMode::Preserve);
    // Deliberately unsorted input order.
    let plan = engine(&store)
        .plan(&spec, &targets(&["b.hnl", "a.hnl"]))
        .unwrap();

    let names: Vec<&str> = plan.changes.iter().map(|c| c.artifact().as_str()).collect();
    assert_eq!(names, ["a.hnl", "b.hnl"]);
}

#[test]
fn edits_are_sorted_deterministically() {
    let store = store_with(&[("a.hnl", "A := 1\nB := 2\nC := 3\n")]);
    let spec = spec(vec![value99_rule()], UnparseMode::None);
    let plan = engine(&store).plan(&spec, &targets(&["a.hnl"])).unwrap();

    let starts: Vec<usize> = plan.edits.iter().map(|e| e.span.unwrap().start).collect();
    assert_eq!(starts, {
        let mut v = starts.clone();
        v.sort();
        v
    });
    assert_eq!(plan.edits.len(), 3);
}

// --- honest declines ------------------------------------------------------

#[test]
fn unresolved_language_skips_the_target() {
    let store = store_with(&[("a.hnl", "A := 1\n")]);
    let spec = spec(vec![value99_rule()], UnparseMode::Preserve);
    // Resolver knows only "other".
    let eng = CodemodEngine::new(&store)
        .with_language_resolver(StaticLanguageResolver::new().with_language("other", lang()));
    let plan = eng.plan(&spec, &targets(&["a.hnl"])).unwrap();

    assert!(plan.changes.is_empty());
    assert!(
        plan.diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::LanguageUnresolved)
    );
}

#[test]
fn unparseable_artifact_is_skipped_with_diagnostic() {
    let store = store_with(&[("bad.hnl", "A := (\n")]);
    let spec = spec(vec![value99_rule()], UnparseMode::Preserve);
    let plan = engine(&store).plan(&spec, &targets(&["bad.hnl"])).unwrap();

    assert!(plan.changes.is_empty());
    assert!(
        plan.diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::ParseError)
    );
}

#[test]
fn invalid_target_index_is_an_error() {
    let store = store_with(&[("a.hnl", "A := 1\n")]);
    let spec = spec(vec![], UnparseMode::None);
    let err = engine(&store)
        .plan(
            &spec,
            &[TargetArtifacts {
                target: 9,
                artifacts: vec![ArtifactRef::new("a.hnl")],
            }],
        )
        .unwrap_err();
    assert!(matches!(err, CodemodError::InvalidTarget(9)));
}
