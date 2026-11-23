//! Byte-splice path tests (PL-004 T-091): `ByteEdit`s — the shape lint
//! `--fix` diagnostics take once mapped through `hnl-tool-lint`'s
//! `fix` module — become `Change::Put`s via
//! `CodemodEngine::plan_byte_edits`, then flow through
//! `hnl-tool-codegen`'s `Engine::apply`/`revert` lifecycle: journaled,
//! drift-checked, revertable.

use hnl_tool_codegen::{ApplyOptions, ArtifactRef, ArtifactStore, Engine, MemoryStore, RunMeta};
use hnl_tool_codemod::{ByteEdit, CodemodEngine, DiagnosticKind};

fn store_with(entries: &[(&str, &str)]) -> MemoryStore {
    let store = MemoryStore::new();
    for (name, src) in entries {
        store
            .write(&ArtifactRef::new(*name), src.as_bytes())
            .unwrap();
    }
    store
}

fn read(store: &MemoryStore, name: &str) -> String {
    let bytes = store.read(&ArtifactRef::new(name)).unwrap().unwrap();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn byte_edits_become_a_journaled_change() {
    let store = store_with(&[("a.hnl", "xxKEEPyyDROPzz")]);
    let edits = vec![
        ByteEdit::new("rule-a", 0, 2, ""),     // delete "xx"
        ByteEdit::new("rule-b", 8, 14, "END"), // "DROPzz" -> "END"
    ];
    let engine = CodemodEngine::new(&store);
    let plan = engine.plan_byte_edits(&ArtifactRef::new("a.hnl"), &edits);

    assert_eq!(plan.changes.len(), 1);
    assert_eq!(
        String::from_utf8(plan.changes[0].content().unwrap().to_vec()).unwrap(),
        "KEEPyyEND"
    );
    assert_eq!(plan.edits.len(), 2);
    assert!(
        plan.edits
            .iter()
            .all(|e| e.rewrite.as_deref() == Some("splice"))
    );
    assert!(plan.edits.iter().any(|e| e.rule_id == "rule-a"));
    assert!(plan.stats.artifacts_changed == 1);
}

#[test]
fn identity_edits_produce_no_change() {
    let store = store_with(&[("a.hnl", "same")]);
    let edits = vec![ByteEdit::new("noop", 0, 0, "")]; // pure insertion of ""
    let plan = CodemodEngine::new(&store).plan_byte_edits(&ArtifactRef::new("a.hnl"), &edits);
    assert!(plan.changes.is_empty(), "spliced text == source");
    assert_eq!(plan.edits.len(), 1, "the edit is still recorded");
}

#[test]
fn overlapping_edits_decline_the_whole_artifact() {
    let store = store_with(&[("a.hnl", "abcdef")]);
    let edits = vec![
        ByteEdit::new("r1", 0, 4, "x"),
        ByteEdit::new("r2", 3, 6, "y"),
    ];
    let plan = CodemodEngine::new(&store).plan_byte_edits(&ArtifactRef::new("a.hnl"), &edits);

    assert!(plan.changes.is_empty(), "no partial splice");
    assert_eq!(read(&store, "a.hnl"), "abcdef", "store untouched");
    assert!(
        plan.diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::SpliceError)
    );
}

#[test]
fn out_of_bounds_edits_decline_with_diagnostic() {
    let store = store_with(&[("a.hnl", "abc")]);
    let edits = vec![ByteEdit::new("r", 1, 99, "x")];
    let plan = CodemodEngine::new(&store).plan_byte_edits(&ArtifactRef::new("a.hnl"), &edits);
    assert!(plan.changes.is_empty());
    assert!(
        plan.diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::SpliceError)
    );
}

#[test]
fn missing_artifact_is_a_read_error_diagnostic() {
    let store = MemoryStore::new();
    let edits = vec![ByteEdit::new("r", 0, 1, "x")];
    let plan = CodemodEngine::new(&store).plan_byte_edits(&ArtifactRef::new("nope.hnl"), &edits);
    assert!(plan.changes.is_empty());
    assert!(
        plan.diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::ReadError)
    );
}

/// The T-091 roundtrip: lint-style fixes → `plan_byte_edits` →
/// `Engine::apply` (journaled, drift-checked) → `Engine::revert`
/// restores the original bytes.
#[test]
fn fixes_flow_through_apply_and_revert() {
    let original = "imp rest ( inner ) tail".to_string();
    // Locate real byte ranges instead of guessing.
    let inner_start = original.find("inner").unwrap();
    let inner_end = inner_start + "inner".len();
    let edits = vec![
        ByteEdit::new("simplify-paren", inner_start - 2, inner_end + 2, "inner"),
        ByteEdit::new("drop-prefix", 0, 4, ""),
    ];
    let expected = apply_expected(&original, &edits);

    let store = store_with(&[("a.hnl", &original)]);
    let codemod = CodemodEngine::new(&store);
    let plan = codemod.plan_byte_edits(&ArtifactRef::new("a.hnl"), &edits);
    assert_eq!(plan.changes.len(), 1);

    // apply through the codegen lifecycle
    let engine = Engine::new(&store);
    let mut meta = RunMeta::new("lint-fix", "hnl-tool-lint-0.1");
    let run_id = meta.new_run_id();
    let report = engine
        .apply(&plan.changes, None, &meta, &ApplyOptions::default())
        .unwrap();
    assert!(report.clean(), "apply failed: {:?}", report.failures);
    assert_eq!(read(&store, "a.hnl"), expected);

    // revert restores the before-image
    let rev = engine.revert(&run_id, &ApplyOptions::default()).unwrap();
    assert!(rev.failures.is_empty());
    assert_eq!(rev.restored, 1);
    assert_eq!(read(&store, "a.hnl"), original);
}

/// Mirror of `apply_byte_edits` for computing the expected text in the
/// roundtrip test (descending splice, same semantics).
fn apply_expected(src: &str, edits: &[ByteEdit]) -> String {
    let mut out = src.to_string();
    let mut sorted: Vec<&ByteEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| e.start);
    for e in sorted.iter().rev() {
        out.replace_range(e.start..e.end, &e.replacement);
    }
    out
}

#[test]
fn lint_fix_out_shape_maps_directly() {
    // The documented host-side mapping: `LintFixOut { start, end,
    // replacement }` + the diagnostic's `rule` name → `ByteEdit`.
    let fix = hnl_tool_codemod::ByteEdit {
        start: 0,
        end: 3,
        replacement: String::new(),
        rule: "unused-import".into(),
    };
    let store = store_with(&[("a.hnl", "imp body")]);
    let plan = CodemodEngine::new(&store).plan_byte_edits(&ArtifactRef::new("a.hnl"), &[fix]);
    assert_eq!(read(&store, "a.hnl"), "imp body", "plan does not write");
    assert_eq!(plan.changes.len(), 1);
    let change_text = String::from_utf8(plan.changes[0].content().unwrap().to_vec()).unwrap();
    assert_eq!(change_text, " body");
}
