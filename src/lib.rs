//! `hnl-engine-codemod` — the declarative codemod engine's spec/model layer.
//!
//! This crate defines the `codemod.yaml` document model (proposal P-002,
//! plan PL-004 Phase 2): the [`CodemodSpec`] top level, its
//! [`CodemodTarget`]s (language + glob + unparse mode) and its
//! [`CodemodRule`]s (scope, engine requirements, match condition, rewrite).
//!
//! Scope of this crate is **data only**: parsing, defaults, and validation.
//! The rewrite engine itself — matching, planning, apply/revert over
//! `ArtifactStore` — lands in later tasks and depends on
//! `engine-transform(-rewrite)`, the parse engines, and
//! `hnl-engine-codegen`. This crate deliberately has **zero kernel deps**:
//! `language` stays a `String` (resolved to `Language` data by the plugin
//! layer), and `match` stays an opaque [`serde_json::Value`] carrying the
//! `TransformCondition` vocabulary shape until the engine binds the real
//! `handl-language` type.

#![warn(missing_docs)]

mod spec;

pub use spec::{
    CodemodRewrite, CodemodRule, CodemodSpec, CodemodTarget, EngineErrorPolicy, RuleScope,
    SpecError, SpecIssue, UnparseMode,
};
