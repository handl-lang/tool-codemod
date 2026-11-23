//! `hnl-engine-codemod` — the declarative codemod engine (P-002 /
//! PL-004 Phase 2, tasks T-081–T-084).
//!
//! Pipeline: `Lift (raw parse) → Transform (declarative/native
//! rewrite) → Materialize (preserve|full|none) → Change set`, over a
//! generic `hnl_engine_codegen::ArtifactStore` and per-target
//! `handl_language::Language` data — no language-specific kind logic
//! lives here (ADR-0044/ADR-0081).
//!
//! - [`CodemodSpec`] / [`CodemodTarget`] / [`CodemodRule`] — the
//!   `codemod.yaml` document model (spec layer, kernel-free `match`
//!   value).
//! - [`CodemodEngine`] — `plan(spec, targets) -> CodemodPlan`: reads
//!   artifacts, raw-parses statements (the ext-transform recipe), runs
//!   compiled rules through `hnl-engine-transform-rewrite`, collects
//!   [`CodemodEdit`]s, and materializes sorted `hnl_engine_codegen::Change`s
//!   for `hnl-engine-codegen`'s apply/revert lifecycle.
//! - [`LanguageResolver`] / [`StaticLanguageResolver`] — per-target
//!   `language:` → `Language` resolution (caller-supplied; fragment
//!   merging is plugin-side).
//! - [`EngineContextProvider`] — the `requires:` seam (`typecheck`,
//!   …): lazy, hash+engine+version cached, `on_engine_error` honored.
//! - [`StatementSplitter`] / [`HandlStatementSplitter`] — top-level
//!   statement chunking (vendored `plugin-ext-diag` algorithm; custom
//!   surfaces plug their own).
//!
//! v1 scope notes (honest declines, never silent): `scope: program`
//! rules are reported [`DiagnosticKind::ProgramScopeUnsupported`];
//! `unparse: none` targets produce edits but no `Change`s;
//! scheduling is a conservative sequential map with deterministic
//! `ArtifactRef`-sorted output.

#![warn(missing_docs)]

mod compact;
mod contexts;
mod engine;
mod match_spec;
mod native;
mod plan;
mod rules;
mod spec;
mod split;

pub use contexts::{
    EngineContextError, EngineContextProvider, EngineContextReport, EngineContextRequest,
};
pub use engine::{
    CodemodEngine, CodemodError, LanguageResolver, StaticLanguageResolver, TargetArtifacts,
};
pub use plan::{CodemodDiagnostic, CodemodEdit, CodemodPlan, CodemodStats, DiagnosticKind, Span};
pub use spec::{
    CodemodRewrite, CodemodRule, CodemodSpec, CodemodTarget, EngineErrorPolicy, RuleScope,
    SpecError, SpecIssue, UnparseMode,
};
pub use split::{HandlStatementSplitter, StatementChunk, StatementSplitter, split_statements};
