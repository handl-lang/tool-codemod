//! Plan output types: [`CodemodPlan`], [`CodemodEdit`],
//! [`CodemodDiagnostic`], [`CodemodStats`].
//!
//! A plan is **data**: edits describe what matched (artifact, source
//! range, rule, before/after snippets); `changes` is the materialized
//! [`Change`] set sorted by [`ArtifactRef`] — ready to hand to
//! `hnl-engine-codegen`'s `Engine::plan`/`apply` for the reconcile
//! half (journal, manifest, drift checks live there, not here).
//!
//! ## `unparse: none` — the honest representation
//!
//! A target with `unparse: none` is plan-only: its rules still run
//! (matches and rewrites are computed so edits carry real
//! before/after data), but **no `Change` is emitted** for those
//! artifacts — there is no content to write, and a `Put` with the
//! original bytes would be a lie about intent. The edits are the
//! plan; `apply` on a none-target spec is a no-op by construction.
//! (Alternative considered — a `Change` with empty content — rejected:
//! it would delete/truncate artifacts under the kernel's semantics.)

use hnl_engine_codegen::{ArtifactRef, Change};
use serde::{Deserialize, Serialize};

/// One matched site — what a rule did (or would do) at one node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodemodEdit {
    /// The artifact the edit applies to.
    pub artifact: ArtifactRef,
    /// The codemod rule id that fired.
    pub rule_id: String,
    /// Byte range of the matched node in the artifact's source
    /// (`None` for nodes without a parser span — e.g. synthesized
    /// nodes; v1 matches always carry spans).
    pub span: Option<Span>,
    /// The matched node's kind (pre-rewrite).
    pub node_kind: String,
    /// Source text of the matched node (`src[span]`), when spanned.
    pub before: Option<String>,
    /// Re-emitted text of the rewritten node. `None` when the node was
    /// deleted, when the rewrite produced no emittable surface, or for
    /// observe-only rules.
    pub after: Option<String>,
    /// `true` when the rule's rewrite was `{ delete: true }`.
    pub deleted: bool,
    /// The rewrite spec text (`"handl:mod.fn"`, `"native:name"`,
    /// `"delete"`-normalized forms); `None` for observe-only rules
    /// (lint-as-codemod: match sites with no rewrite).
    pub rewrite: Option<String>,
}

/// A byte range — mirrors `handl_language::Span` without forcing the
/// kernel type into the wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// Inclusive start byte offset.
    pub start: usize,
    /// Exclusive end byte offset.
    pub end: usize,
}

impl From<handl_language::Span> for Span {
    fn from(s: handl_language::Span) -> Self {
        Self {
            start: s.start,
            end: s.end,
        }
    }
}

/// Why the engine produced (or skipped) something — never silent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiagnosticKind {
    /// The artifact could not be read from the store.
    ReadError,
    /// The artifact is not valid UTF-8.
    NotUtf8,
    /// Lex/parse failed — the artifact is skipped whole (codemod's
    /// `on_error: skip` default for unparseable files).
    ParseError,
    /// A `requires:` engine has no registered provider, or the
    /// provider failed, and `on_engine_error: skip` applied.
    EngineUnavailable,
    /// A `requires:` engine failed under `on_engine_error: fail` —
    /// aborts the run via `crate::CodemodError`.
    EngineFailed,
    /// A rule's `match:` failed to decode — the rule is skipped.
    RuleInvalid,
    /// A rule's rewrite failed inside the pass — the artifact's
    /// statement is skipped, the run continues.
    RewriteError,
    /// Re-emission (`preserve`/`full`) failed for a statement.
    EmitError,
    /// `scope: program` rules are declared but not implemented in v1 —
    /// reported once per rule, never silently dropped.
    ProgramScopeUnsupported,
    /// No `LanguageResolver` wired (or resolution failed) — the whole
    /// target is skipped.
    LanguageUnresolved,
    /// A `language:` target had no artifacts supplied for it.
    NoArtifacts,
    /// A byte-splice edit batch ([`crate::ByteEdit`]) failed validation —
    /// overlap, out-of-bounds, or a non-char-boundary offset. The whole
    /// artifact is skipped; a splice is never partially applied.
    SpliceError,
}

/// One diagnostic: where, what kind, and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodemodDiagnostic {
    /// The artifact involved, when applicable.
    pub artifact: Option<ArtifactRef>,
    /// The rule involved, when applicable.
    pub rule_id: Option<String>,
    /// The `requires:` engine involved, when applicable.
    pub engine: Option<String>,
    /// What happened.
    pub kind: DiagnosticKind,
    /// Human-readable detail (provider/error text verbatim).
    pub message: String,
}

impl CodemodDiagnostic {
    /// Build a diagnostic with only mandatory fields.
    pub fn new(kind: DiagnosticKind, message: impl Into<String>) -> Self {
        Self {
            artifact: None,
            rule_id: None,
            engine: None,
            kind,
            message: message.into(),
        }
    }

    /// Attach an artifact.
    pub fn at_artifact(mut self, artifact: ArtifactRef) -> Self {
        self.artifact = Some(artifact);
        self
    }

    /// Attach a rule id.
    pub fn at_rule(mut self, rule_id: impl Into<String>) -> Self {
        self.rule_id = Some(rule_id.into());
        self
    }

    /// Attach an engine name.
    pub fn at_engine(mut self, engine: impl Into<String>) -> Self {
        self.engine = Some(engine.into());
        self
    }
}

/// Run statistics — counts only, all deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodemodStats {
    /// Artifacts supplied across all targets.
    pub artifacts_seen: usize,
    /// Artifacts successfully parsed (at least one statement).
    pub artifacts_parsed: usize,
    /// Statements raw-parsed.
    pub statements: usize,
    /// Rewrite sites recorded (`trace.rewrites`).
    pub rewrites: usize,
    /// Total edits (rewrite + observe matches).
    pub edits: usize,
    /// Artifacts that produced at least one `Change`.
    pub artifacts_changed: usize,
    /// `EngineContextProvider::provide` invocations (cache misses).
    pub engine_context_calls: usize,
    /// Rules skipped per artifact by `on_engine_error: skip`.
    pub rules_skipped_engine: usize,
}

/// The result of [`CodemodEngine::plan`](crate::CodemodEngine::plan):
/// edits, materialized changes, diagnostics, stats.
#[derive(Debug)]
pub struct CodemodPlan {
    /// Codemod name (spec).
    pub name: String,
    /// Codemod version (spec) — part of downstream cache keys.
    pub version: u32,
    /// Every match site, sorted by `(artifact, span.start, rule_id)`.
    pub edits: Vec<CodemodEdit>,
    /// Desired-state `Change`s, sorted by `ArtifactRef`. Empty for
    /// plan-only (`unparse: none`) targets.
    pub changes: Vec<Change>,
    /// Honest declines and notices.
    pub diagnostics: Vec<CodemodDiagnostic>,
    /// Counts.
    pub stats: CodemodStats,
}

impl CodemodPlan {
    /// True when the plan carries no `Change`s.
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// True when any diagnostic was emitted.
    pub fn has_diagnostics(&self) -> bool {
        !self.diagnostics.is_empty()
    }
}
