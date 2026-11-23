//! Spec-model tests: representative `codemod.yaml`, defaults, and the
//! validation pass.

use hnl_engine_codemod::{
    CodemodRewrite, CodemodSpec, EngineErrorPolicy, RuleScope, SpecError, UnparseMode,
};

/// Mirrors the proposal P-002 example, with `match` spelled in the real
/// `TransformCondition` serde vocabulary (externally-tagged enum).
const VALID: &str = r#"
name: rename-foo
version: 1
targets:
  - language: handl
    glob: "src/**/*.hnl"
    unparse: preserve
  - language: python
    glob: "scripts/**/*.py"
rules:
  - id: foo-to-bar
    scope: file
    requires: []
    match:
      PayloadEq: { path: "name", value: "foo" }
    rewrite: "handl:migrations.foo_to_bar"
  - id: drop-deprecated
    scope: program
    requires: [typecheck]
    on_engine_error: fail
    match:
      Or:
        - PayloadEq: { path: "name", value: { $regex: "deprecated_.*" } }
        - AnyChildKind: { kind: "deprecated-call" }
    rewrite: { delete: true }
"#;

#[test]
fn parses_representative_spec() {
    let spec = CodemodSpec::parse(VALID).expect("valid spec");
    assert_eq!(spec.name, "rename-foo");
    assert_eq!(spec.version, 1);
    assert_eq!(spec.targets.len(), 2);
    assert_eq!(spec.rules.len(), 2);

    let t0 = &spec.targets[0];
    assert_eq!(t0.language, "handl");
    assert_eq!(t0.glob, "src/**/*.hnl");
    assert_eq!(t0.unparse, UnparseMode::Preserve);

    // Second target omits `unparse` → default None (plan-only).
    assert_eq!(spec.targets[1].unparse, UnparseMode::None);

    let r0 = &spec.rules[0];
    assert_eq!(r0.id, "foo-to-bar");
    assert_eq!(r0.scope, RuleScope::File);
    assert_eq!(r0.on_engine_error, EngineErrorPolicy::Skip);
    assert_eq!(r0.requires, Vec::<String>::new());
    assert!(r0.condition.is_some());
    assert_eq!(
        r0.rewrite,
        Some(CodemodRewrite::Named(
            "handl:migrations.foo_to_bar".to_string()
        ))
    );

    let r1 = &spec.rules[1];
    assert_eq!(r1.scope, RuleScope::Program);
    assert_eq!(r1.on_engine_error, EngineErrorPolicy::Fail);
    assert_eq!(r1.requires, vec!["typecheck".to_string()]);
    assert_eq!(r1.rewrite, Some(CodemodRewrite::Delete { delete: true }));
}

#[test]
fn defaults_are_applied() {
    let src = r#"
name: minimal
version: 2
targets:
  - language: handl
    glob: "**/*.hnl"
rules:
  - id: only-rule
    rewrite: "native:lint_fix"
"#;
    let spec = CodemodSpec::parse(src).expect("minimal spec");
    let rule = &spec.rules[0];
    assert_eq!(rule.scope, RuleScope::File);
    assert_eq!(rule.on_engine_error, EngineErrorPolicy::Skip);
    assert!(rule.requires.is_empty());
    assert!(rule.condition.is_none());
    assert_eq!(spec.targets[0].unparse, UnparseMode::None);
}

#[test]
fn unparse_modes_round_trip() {
    for (spelling, mode) in [
        ("preserve", UnparseMode::Preserve),
        ("full", UnparseMode::Full),
        ("none", UnparseMode::None),
    ] {
        let src = format!(
            "name: x\nversion: 1\ntargets:\n  - language: handl\n    glob: g\n    unparse: {spelling}\n"
        );
        let spec = CodemodSpec::parse(&src).unwrap();
        assert_eq!(spec.targets[0].unparse, mode);
    }
}

#[test]
fn validation_rejects_empty_targets() {
    let src = "name: x\nversion: 1\ntargets: []\nrules: []\n";
    match CodemodSpec::parse(src) {
        Err(SpecError::Invalid(issues)) => {
            assert!(issues.iter().any(|i| i.path == "targets"));
        }
        other => panic!("expected validation error, got {other:?}"),
    }
}

#[test]
fn validation_rejects_duplicate_rule_ids() {
    let src = r#"
name: x
version: 1
targets:
  - language: handl
    glob: "**/*.hnl"
rules:
  - id: same
    rewrite: "native:a"
  - id: same
    rewrite: "native:b"
"#;
    match CodemodSpec::parse(src) {
        Err(SpecError::Invalid(issues)) => {
            assert!(
                issues
                    .iter()
                    .any(|i| i.path == "rules[1].id" && i.message.contains("duplicate"))
            );
        }
        other => panic!("expected validation error, got {other:?}"),
    }
}

#[test]
fn validation_reports_unknown_enum_values_with_the_value() {
    let src = r#"
name: x
version: 1
targets:
  - language: handl
    glob: "**/*.hnl"
    unparse: sideways
rules:
  - id: r
    scope: galaxy
    on_engine_error: explode
    rewrite: "native:a"
"#;
    match CodemodSpec::parse(src) {
        Err(SpecError::Invalid(issues)) => {
            assert!(
                issues
                    .iter()
                    .any(|i| i.path == "targets[0].unparse" && i.message.contains("sideways"))
            );
            assert!(
                issues
                    .iter()
                    .any(|i| i.path == "rules[0].scope" && i.message.contains("galaxy"))
            );
            assert!(
                issues
                    .iter()
                    .any(|i| i.path == "rules[0].on_engine_error" && i.message.contains("explode"))
            );
        }
        other => panic!("expected validation error, got {other:?}"),
    }
}

#[test]
fn validation_collects_multiple_issues() {
    let src = r#"
name: ""
version: 1
targets:
  - language: ""
    glob: ""
rules:
  - id: ""
    rewrite: { delete: false }
"#;
    match CodemodSpec::parse(src) {
        Err(SpecError::Invalid(issues)) => {
            // name, language, glob, rule id, delete:false — all reported.
            assert!(issues.len() >= 5, "issues: {issues:?}");
            assert!(issues.iter().any(|i| i.path == "name"));
            assert!(issues.iter().any(|i| i.path == "targets[0].language"));
            assert!(issues.iter().any(|i| i.path == "targets[0].glob"));
            assert!(issues.iter().any(|i| i.path == "rules[0].id"));
            assert!(issues.iter().any(|i| i.path == "rules[0].rewrite"));
        }
        other => panic!("expected validation error, got {other:?}"),
    }
}

#[test]
fn malformed_yaml_is_a_structured_error_not_a_panic() {
    match CodemodSpec::parse("name: [unclosed") {
        Err(SpecError::Yaml(_)) => {}
        other => panic!("expected YAML error, got {other:?}"),
    }
}

#[test]
fn missing_required_fields_is_a_structured_error() {
    match CodemodSpec::parse("version: 1\n") {
        Err(SpecError::Yaml(_)) => {}
        other => panic!("expected YAML error, got {other:?}"),
    }
}

#[test]
fn rule_without_rewrite_is_allowed() {
    // Lint-as-codemod: a rule may declare match/requires and emit
    // diagnostics without a rewrite.
    let src = r#"
name: diagnostics-only
version: 1
targets:
  - language: handl
    glob: "**/*.hnl"
rules:
  - id: flag-deprecated
    requires: [typecheck]
    match:
      AnyChildKind: { kind: "deprecated-call" }
"#;
    let spec = CodemodSpec::parse(src).expect("diagnostic-only rule");
    assert!(spec.rules[0].rewrite.is_none());
}

#[test]
fn native_yaml_tag_is_accepted_and_normalized() {
    // The proposal's `!Native "name"` spelling: serde_yaml exposes it as a
    // tagged scalar; the spec model normalizes it to `native:<name>`.
    let src = "name: x\nversion: 1\ntargets:\n  - language: handl\n    glob: g\nrules:\n  - id: r\n    rewrite: !Native \"foo_to_bar\"\n";
    let spec = CodemodSpec::parse(src).expect("tagged native rewrite");
    assert_eq!(
        spec.rules[0].rewrite,
        Some(CodemodRewrite::Named("native:foo_to_bar".to_string()))
    );
}

#[test]
fn rewrite_map_with_unknown_key_is_a_yaml_error() {
    let src = "name: x\nversion: 1\ntargets:\n  - language: handl\n    glob: g\nrules:\n  - id: r\n    rewrite: { replace: true }\n";
    match CodemodSpec::parse(src) {
        Err(SpecError::Yaml(_)) => {}
        other => panic!("expected YAML error, got {other:?}"),
    }
}

#[test]
fn load_reads_from_disk() {
    let dir = std::env::temp_dir().join(format!("codemod-spec-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("codemod.yaml");
    std::fs::write(&path, VALID).unwrap();
    let spec = CodemodSpec::load(&path).expect("load");
    assert_eq!(spec.name, "rename-foo");
    std::fs::remove_dir_all(&dir).ok();
}
