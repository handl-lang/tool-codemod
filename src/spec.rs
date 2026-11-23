//! The `codemod.yaml` spec model: typed structs, YAML entry points, and the
//! validation pass.
//!
//! Document shape (proposal P-002):
//!
//! ```yaml
//! name: rename-foo
//! version: 1
//! targets:
//!   - language: handl
//!     glob: "src/**/*.hnl"
//!     unparse: preserve        # preserve | full | none (default none)
//! rules:
//!   - id: foo-to-bar
//!     scope: file              # file | program (default file)
//!     requires: [typecheck]    # engine contexts, run lazily per artifact
//!     on_engine_error: skip    # skip | fail (default skip)
//!     match: { ... }           # TransformCondition vocabulary (opaque here)
//!     rewrite: "handl:migrations.foo_to_bar"   # or { delete: true }
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

/// A parsed `codemod.yaml` — the top-level spec document.
///
/// Obtain a validated spec via [`CodemodSpec::load`] or
/// [`CodemodSpec::parse`]; [`CodemodSpec::from_yaml`] parses without
/// validating.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodemodSpec {
    /// Codemod identifier, e.g. `rename-foo`. Used in plan artifacts,
    /// journals, and cache keys.
    pub name: String,
    /// Codemod version. Part of the plan-cache key
    /// `(codemod version, rule-set hash, artifact hash)` — bump it when the
    /// rule set changes semantics.
    pub version: u32,
    /// Artifact sets the codemod applies to. Must be non-empty (enforced by
    /// [`CodemodSpec::validate`]).
    #[serde(default)]
    pub targets: Vec<CodemodTarget>,
    /// Rewrite rules. May be empty — plugin-contributed
    /// `CodemodRuleProvider`s can supply rules at run time.
    #[serde(default)]
    pub rules: Vec<CodemodRule>,
}

/// One target of a codemod: a glob over artifacts plus the `Language` they
/// parse/rewrite under.
///
/// `language` is kept as a [`String`] on purpose: the plugin layer resolves
/// it to `handl_language::Language` data per target (ADR-0044 — a new
/// language is a data change, never an engine recompile). This crate has no
/// kernel dep.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodemodTarget {
    /// Name of the `Language` fragment set to parse/rewrite under, e.g.
    /// `handl`. Resolved by the plugin layer, not here.
    pub language: String,
    /// Glob selecting the artifacts this target covers, e.g.
    /// `"src/**/*.hnl"`. Interpreted by the plugin layer against the
    /// `ArtifactStore`.
    pub glob: String,
    /// How rewritten ASTs are emitted back to source for this target.
    /// Defaults to [`UnparseMode::None`] (plan-only).
    #[serde(default)]
    pub unparse: UnparseMode,
}

/// How a target's rewritten AST is emitted back to source.
///
/// Deserialized leniently: an unrecognized spelling is preserved in
/// [`UnparseMode::Unknown`] so [`CodemodSpec::validate`] can report the
/// offending value instead of failing at parse time.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UnparseMode {
    /// Re-emit only the changed spans (fmt's `PreserveEmitter` precedent).
    Preserve,
    /// Re-emit the whole artifact from the rewritten AST.
    Full,
    /// Do not emit source — the codemod is plan-only for this target
    /// (edits exist as data; `apply` is refused). This is the default.
    #[default]
    None,
    /// An unrecognized `unparse:` value, kept verbatim for validation
    /// errors. Never produced by hand; always a spec bug.
    Unknown(String),
}

impl Serialize for UnparseMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Preserve => "preserve",
            Self::Full => "full",
            Self::None => "none",
            Self::Unknown(s) => s.as_str(),
        })
    }
}

impl<'de> Deserialize<'de> for UnparseMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "preserve" => Self::Preserve,
            "full" => Self::Full,
            "none" => Self::None,
            other => Self::Unknown(other.to_string()),
        })
    }
}

/// Rule scheduling scope (P-002 "derived scheduling").
///
/// `file`-scope rules map in parallel per artifact; `program`-scope rules
/// (cross-file rewrites) join after a barrier in the merge phase. v1 ships
/// file-scope only — the declaration exists so scheduling is *derived*,
/// never hardcoded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RuleScope {
    /// The rule rewrites within a single artifact. Default.
    #[default]
    File,
    /// The rule may rewrite across artifacts (needs the merge phase).
    Program,
    /// An unrecognized `scope:` value, kept verbatim for validation errors.
    Unknown(String),
}

impl Serialize for RuleScope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::File => "file",
            Self::Program => "program",
            Self::Unknown(s) => s.as_str(),
        })
    }
}

impl<'de> Deserialize<'de> for RuleScope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "file" => Self::File,
            "program" => Self::Program,
            other => Self::Unknown(other.to_string()),
        })
    }
}

/// What the engine does when a `requires:` engine fails on an artifact.
///
/// Applies to *any* entry in [`CodemodRule::requires`] — no per-engine
/// special-casing. Default is [`EngineErrorPolicy::Skip`]: an honest decline
/// per artifact (diagnostic recorded), never a silent fallback.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EngineErrorPolicy {
    /// Skip the rule for that artifact and record a diagnostic.
    #[default]
    Skip,
    /// Fail the whole codemod run.
    Fail,
    /// An unrecognized `on_engine_error:` value, kept verbatim for
    /// validation errors.
    Unknown(String),
}

impl Serialize for EngineErrorPolicy {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Skip => "skip",
            Self::Fail => "fail",
            Self::Unknown(s) => s.as_str(),
        })
    }
}

impl<'de> Deserialize<'de> for EngineErrorPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "skip" => Self::Skip,
            "fail" => Self::Fail,
            other => Self::Unknown(other.to_string()),
        })
    }
}

/// One rewrite rule inside a [`CodemodSpec`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodemodRule {
    /// Unique rule identifier, e.g. `foo-to-bar`. Referenced by
    /// `CodemodEdit`s and journal entries; must be unique within the spec
    /// (enforced by [`CodemodSpec::validate`]).
    pub id: String,
    /// Scheduling scope — [`RuleScope::File`] (default) or
    /// [`RuleScope::Program`].
    #[serde(default)]
    pub scope: RuleScope,
    /// Engine contexts the rule needs (e.g. `typecheck`, `scope`, …). The
    /// plugin layer runs the named engines per artifact — hash-cached,
    /// lazily, only when a rule asks — and exposes their reports to
    /// match/rewrite. Empty = no engine context required.
    #[serde(default)]
    pub requires: Vec<String>,
    /// Behavior when any `requires:` engine fails. Default
    /// [`EngineErrorPolicy::Skip`].
    #[serde(default)]
    pub on_engine_error: EngineErrorPolicy,
    /// The match condition — **opaque** at this layer.
    ///
    /// Carries the `handl-language` `TransformCondition` vocabulary shape
    /// (the externally-tagged enum serde form: `KindAt`, `AnyChildKind`,
    /// `PayloadEq`, `ChildPayloadEq`, `ChildCount`, `AttrEq`, `SameAttr`,
    /// `And`, `Or`, `Not`), kept as a [`serde_json::Value`] so this crate
    /// stays kernel-free. The rewrite engine binds it to the real
    /// `TransformCondition` type. `None` = the rule is unconditional.
    #[serde(rename = "match", default)]
    pub condition: Option<Value>,
    /// What fires when the rule matches. `None` = the rule only emits
    /// diagnostics (the lint-as-codemod path: lint rules exposed as codemod
    /// rules that produce `Change`s, or plan entries, without a rewrite).
    #[serde(default)]
    pub rewrite: Option<CodemodRewrite>,
}

/// A rule's rewrite target.
///
/// Three YAML spellings:
///
/// - `"handl:<module>.<fn>"` — resolves a `.hnl` rewrite function through
///   the `HandlNativeRewrite` resolver;
/// - `"native:<name>"` or the proposal's `!Native "name"` YAML tag — names a
///   registered native rewrite (the tag form is normalized to
///   `native:<name>` at parse time);
/// - `{ delete: true }` — deletes the matched node.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum CodemodRewrite {
    /// A named rewrite: `handl:<module>.<fn>` or `native:<name>`.
    Named(String),
    /// `{ delete: true }` — delete the matched node. `delete` must be
    /// `true` (enforced by [`CodemodSpec::validate`]).
    Delete {
        /// Must be `true`; `false` is a validation error.
        delete: bool,
    },
}

impl<'de> Deserialize<'de> for CodemodRewrite {
    /// Custom impl (not `#[serde(untagged)]`) so the `!Native "name"` YAML
    /// tag survives: serde_yaml exposes tags through
    /// [`serde_yaml::Value::Tagged`], which untagged enums reject.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_yaml::Value::deserialize(deserializer)?;
        Self::from_yaml_value(&value).map_err(serde::de::Error::custom)
    }
}

impl CodemodRewrite {
    fn from_yaml_value(value: &serde_yaml::Value) -> Result<Self, String> {
        use serde_yaml::Value as Y;
        match value {
            Y::String(s) => Ok(Self::Named(s.clone())),
            Y::Tagged(tagged) => {
                let tag = tagged.tag.to_string();
                match tag.as_str() {
                    "!Native" | "!native" => match tagged.value.as_str() {
                        Some(name) => Ok(Self::Named(format!("native:{name}"))),
                        None => Err(format!("rewrite tag {tag} expects a string name")),
                    },
                    other => Err(format!("unknown rewrite tag {other}")),
                }
            }
            Y::Mapping(map) if map.len() == 1 => {
                let (key, val) = map.iter().next().expect("len checked");
                if key.as_str() == Some("delete") {
                    match val.as_bool() {
                        Some(delete) => Ok(Self::Delete { delete }),
                        None => Err("rewrite delete expects a bool".into()),
                    }
                } else {
                    Err(format!("unknown rewrite key {key:?} (expected delete)"))
                }
            }
            other => Err(format!(
                "rewrite must be a string or {{ delete: true }}, got {other:?}"
            )),
        }
    }
}

/// One validation finding: where in the spec and what is wrong.
///
/// `path` is a JSONPath-ish locator (`targets[0].glob`, `rules[2].id`),
/// `message` a human-readable explanation. [`CodemodSpec::validate`]
/// collects *all* issues rather than stopping at the first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecIssue {
    /// Location of the problem, e.g. `rules[0].id` or `targets`.
    pub path: String,
    /// What is wrong, e.g. `duplicate rule id "foo-to-bar"`.
    pub message: String,
}

impl fmt::Display for SpecIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Errors from [`CodemodSpec::load`] / [`CodemodSpec::parse`].
#[derive(Debug)]
pub enum SpecError {
    /// The YAML did not parse or did not match the spec shape
    /// (missing required field, wrong type, malformed enum spelling that is
    /// not a string, etc.).
    Yaml(serde_yaml::Error),
    /// The spec file could not be read from disk.
    Io {
        /// The path that failed to read.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The YAML parsed but failed [`CodemodSpec::validate`].
    Invalid(Vec<SpecIssue>),
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Yaml(e) => write!(f, "codemod spec YAML error: {e}"),
            Self::Io { path, source } => {
                write!(f, "cannot read codemod spec {}: {source}", path.display())
            }
            Self::Invalid(issues) => {
                write!(
                    f,
                    "codemod spec failed validation ({} issue{}): ",
                    issues.len(),
                    if issues.len() == 1 { "" } else { "s" }
                )?;
                for (i, issue) in issues.iter().enumerate() {
                    if i > 0 {
                        write!(f, "; ")?;
                    }
                    write!(f, "{issue}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for SpecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Yaml(e) => Some(e),
            Self::Io { source, .. } => Some(source),
            Self::Invalid(_) => None,
        }
    }
}

impl From<serde_yaml::Error> for SpecError {
    fn from(e: serde_yaml::Error) -> Self {
        Self::Yaml(e)
    }
}

impl CodemodSpec {
    /// Load and validate a `codemod.yaml` from disk.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, SpecError> {
        let src = std::fs::read_to_string(path.as_ref()).map_err(|source| SpecError::Io {
            path: path.as_ref().to_path_buf(),
            source,
        })?;
        Self::parse(&src)
    }

    /// Parse and validate a `codemod.yaml` document.
    ///
    /// Equivalent to [`CodemodSpec::from_yaml`] followed by
    /// [`CodemodSpec::validate`]; returns [`SpecError::Invalid`] with all
    /// issues when validation fails.
    pub fn parse(src: &str) -> Result<Self, SpecError> {
        let spec = Self::from_yaml(src)?;
        spec.validate().map_err(SpecError::Invalid)?;
        Ok(spec)
    }

    /// Parse a `codemod.yaml` document **without** validating.
    ///
    /// Useful for diagnostics that want to inspect a broken spec; prefer
    /// [`CodemodSpec::parse`] / [`CodemodSpec::load`] otherwise.
    pub fn from_yaml(src: &str) -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_str(src)
    }

    /// Validate the spec, returning every issue found.
    ///
    /// Checks: non-empty `name`; at least one target; per-target non-empty
    /// `language`/`glob` and a recognized `unparse` value; per-rule non-empty
    /// unique `id`, recognized `scope`/`on_engine_error` values, non-empty
    /// `requires` entries, and a well-formed `rewrite`.
    pub fn validate(&self) -> Result<(), Vec<SpecIssue>> {
        let mut issues = Vec::new();

        if self.name.trim().is_empty() {
            issues.push(SpecIssue {
                path: "name".into(),
                message: "codemod name must not be empty".into(),
            });
        }

        if self.targets.is_empty() {
            issues.push(SpecIssue {
                path: "targets".into(),
                message: "spec declares no targets".into(),
            });
        }
        for (i, target) in self.targets.iter().enumerate() {
            let base = format!("targets[{i}]");
            if target.language.trim().is_empty() {
                issues.push(SpecIssue {
                    path: format!("{base}.language"),
                    message: "target language must not be empty".into(),
                });
            }
            if target.glob.trim().is_empty() {
                issues.push(SpecIssue {
                    path: format!("{base}.glob"),
                    message: "target glob must not be empty".into(),
                });
            }
            if let UnparseMode::Unknown(v) = &target.unparse {
                issues.push(SpecIssue {
                    path: format!("{base}.unparse"),
                    message: format!("unknown unparse value {v:?} (expected preserve|full|none)"),
                });
            }
        }

        let mut seen = BTreeSet::new();
        for (i, rule) in self.rules.iter().enumerate() {
            let base = format!("rules[{i}]");
            if rule.id.trim().is_empty() {
                issues.push(SpecIssue {
                    path: format!("{base}.id"),
                    message: "rule id must not be empty".into(),
                });
            } else if !seen.insert(rule.id.as_str()) {
                issues.push(SpecIssue {
                    path: format!("{base}.id"),
                    message: format!("duplicate rule id {:?}", rule.id),
                });
            }
            if let RuleScope::Unknown(v) = &rule.scope {
                issues.push(SpecIssue {
                    path: format!("{base}.scope"),
                    message: format!("unknown scope value {v:?} (expected file|program)"),
                });
            }
            if let EngineErrorPolicy::Unknown(v) = &rule.on_engine_error {
                issues.push(SpecIssue {
                    path: format!("{base}.on_engine_error"),
                    message: format!("unknown on_engine_error value {v:?} (expected skip|fail)"),
                });
            }
            for (j, engine) in rule.requires.iter().enumerate() {
                if engine.trim().is_empty() {
                    issues.push(SpecIssue {
                        path: format!("{base}.requires[{j}]"),
                        message: "requires entry must not be empty".into(),
                    });
                }
            }
            match &rule.rewrite {
                Some(CodemodRewrite::Named(s)) if s.trim().is_empty() => {
                    issues.push(SpecIssue {
                        path: format!("{base}.rewrite"),
                        message: "rewrite name must not be empty".into(),
                    });
                }
                Some(CodemodRewrite::Delete { delete: false }) => {
                    issues.push(SpecIssue {
                        path: format!("{base}.rewrite"),
                        message:
                            "rewrite {{ delete: false }} is meaningless; use true or omit rewrite"
                                .into(),
                    });
                }
                _ => {}
            }
        }

        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }
}
