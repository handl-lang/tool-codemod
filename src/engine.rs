//! [`CodemodEngine`] — the per-artifact codemod pipeline (T-081–T-084).
//!
//! ```text
//! spec.targets × supplied artifacts
//!   → resolve Language (LanguageResolver, per target)
//!   → read bytes (ArtifactStore) → UTF-8 → ContentHash
//!   → gate rules on `requires:` (EngineContextProvider, hash-cached,
//!     lazy, on_engine_error: skip|fail)
//!   → split statements (StatementSplitter) → raw-parse each
//!     (lex_pattern + PrattEngine, spans re-offset — the ext-transform
//!     recipe, no desugar)
//!   → RewriteEngine pass over compiled TransformDefs
//!     (match = TransformCondition via dispatch; rewrite = Native
//!     resolver / internal delete tombstone)
//!   → strip tombstones → CodemodEdits (rewrite trace + observe
//!     matches) → unparse per target (preserve|full|none)
//!   → Change::Put sorted by ArtifactRef
//! ```
//!
//! ## Scheduling (T-084)
//!
//! Phases are *derived* from `scope:`: `file`-scope rules map over
//! artifacts; `program`-scope rules join after a barrier — **v1
//! reports them as [`DiagnosticKind::ProgramScopeUnsupported`] and
//! runs file rules only** (the proposal defers cross-file rewrite
//! implementation). Execution is a conservative sequential map:
//! `ArtifactStore` is not `Sync`-bound, so a parallel map is a later,
//! opt-in phase — output ordering (`ArtifactRef`-sorted `Change`s,
//! sorted edits) is already deterministic either way.
//!
//! Journal/manifest bookkeeping is *not* here: the returned
//! [`Change`] set feeds `hnl-tool-codegen`'s `Engine::apply`, which
//! owns write-ahead journaling, manifests, and revert.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use handl_language::{Ast, AstRef, Language};
use hnl_tool_codegen::{ArtifactRef, ArtifactStore, Change, ContentHash};
use hnl_tool_fmt::{FormatEngine, PreserveEmitter};
use hnl_tool_parse::ParserEngine;
use hnl_tool_transform::native::NativeRewrite;
use hnl_tool_transform_rewrite::RewriteEngine;

use crate::compact::strip_tombstones;
use crate::contexts::{EngineContextCache, EngineContextProvider, context_for};
use crate::native::{TOMBSTONE_KIND, codemod_natives};
use crate::plan::{
    CodemodDiagnostic, CodemodEdit, CodemodPlan, CodemodStats, DiagnosticKind, Span,
};
use crate::rules::{
    CODEMOD_PASS, CodemodRuleSource, CompiledRule, compile_rules, observe_matches, reachable_from,
};
use crate::spec::{CodemodSpec, CodemodTarget, EngineErrorPolicy, RuleScope, UnparseMode};
use crate::splice::{ByteEdit, apply_byte_edits};
use crate::split::{HandlStatementSplitter, StatementChunk, StatementSplitter};

/// The artifacts one target covers — glob resolution is the caller's
/// job (the `ArtifactStore` trait cannot enumerate; the plugin layer
/// matches `target.glob` and supplies refs, sorted or not — the engine
/// sorts and dedups).
#[derive(Debug, Clone)]
pub struct TargetArtifacts {
    /// Index into [`CodemodSpec::targets`].
    pub target: usize,
    /// Artifact refs the target's glob matched.
    pub artifacts: Vec<ArtifactRef>,
}

/// Resolves a `CodemodTarget.language` name to `Language` data
/// (ADR-0044 — a new language is a data change; the engine holds no
/// kind strings and no language registry of its own).
///
/// The canonical kit default is supplied by the *caller* (the plugin
/// layer knows the kit); fragment-merge for non-canonical languages
/// is likewise plugin-side — the engine just consumes `Arc<Language>`.
pub trait LanguageResolver: Send + Sync {
    /// Resolve `name` (e.g. `"handl"`) to a `Language`.
    ///
    /// # Errors
    ///
    /// `Err(message)` — reported as a [`DiagnosticKind::LanguageUnresolved`]
    /// diagnostic; the target is skipped, other targets continue.
    fn resolve(&self, name: &str) -> Result<Arc<Language>, String>;
}

/// A [`LanguageResolver`] over a fixed name → `Language` map — the
/// canonical default for tests and single-language codemods.
#[derive(Debug, Default)]
pub struct StaticLanguageResolver {
    languages: BTreeMap<String, Arc<Language>>,
}

impl StaticLanguageResolver {
    /// An empty resolver.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `lang` under `name` (builder style).
    pub fn with_language(
        mut self,
        name: impl Into<String>,
        lang: impl Into<Arc<Language>>,
    ) -> Self {
        self.languages.insert(name.into(), lang.into());
        self
    }
}

impl LanguageResolver for StaticLanguageResolver {
    fn resolve(&self, name: &str) -> Result<Arc<Language>, String> {
        self.languages
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no language registered for {name:?}"))
    }
}

/// Errors that abort the whole `plan` call — everything else is a
/// per-artifact/per-rule diagnostic (honest declines, never silent).
#[derive(Debug)]
pub enum CodemodError {
    /// A `requires:` engine failed under `on_engine_error: fail`.
    Engine {
        /// The artifact being processed.
        artifact: ArtifactRef,
        /// The rule whose requirement failed.
        rule_id: String,
        /// The `requires:` engine name.
        engine: String,
        /// The provider's error text.
        message: String,
    },
    /// A `TargetArtifacts.target` index is out of range for the spec.
    InvalidTarget(usize),
}

impl std::fmt::Display for CodemodError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Engine {
                artifact,
                rule_id,
                engine,
                message,
            } => write!(
                f,
                "codemod rule '{rule_id}' requires engine '{engine}' on {artifact}: {message}"
            ),
            Self::InvalidTarget(i) => write!(f, "target index {i} out of range for spec"),
        }
    }
}

impl std::error::Error for CodemodError {}

/// The codemod engine over any [`ArtifactStore`].
///
/// Cheap to construct; all wiring is builder-style. `plan` is
/// `&self` — per-run state (the engine-context cache) lives inside the
/// call.
pub struct CodemodEngine<'s, S: ArtifactStore + ?Sized> {
    store: &'s S,
    languages: Option<Arc<dyn LanguageResolver>>,
    splitter: Arc<dyn StatementSplitter>,
    natives: Option<Arc<dyn NativeRewrite>>,
    contexts: HashMap<String, Arc<dyn EngineContextProvider>>,
}

impl<'s, S: ArtifactStore + ?Sized> CodemodEngine<'s, S> {
    /// An engine over `store` with no resolver/splitter/contexts wired
    /// — the [`HandlStatementSplitter`] default applies.
    pub fn new(store: &'s S) -> Self {
        Self {
            store,
            languages: None,
            splitter: Arc::new(HandlStatementSplitter),
            natives: None,
            contexts: HashMap::new(),
        }
    }

    /// Wire the `language:` name resolver (required for any target to
    /// run — see [`LanguageResolver`]).
    pub fn with_language_resolver(mut self, resolver: impl LanguageResolver + 'static) -> Self {
        self.languages = Some(Arc::new(resolver));
        self
    }

    /// Wire a `LanguageResolver` already behind `Arc`.
    pub fn with_language_resolver_arc(mut self, resolver: Arc<dyn LanguageResolver>) -> Self {
        self.languages = Some(resolver);
        self
    }

    /// Wire a custom statement splitter (non-canonical surfaces).
    pub fn with_statement_splitter(mut self, splitter: impl StatementSplitter + 'static) -> Self {
        self.splitter = Arc::new(splitter);
        self
    }

    /// Wire the rewrite resolver — the `handl:<mod>.<fn>` `.hnl`
    /// adapter and/or registered `native:` rewrites (the plugin layer
    /// supplies it; same seam as lint natives). Without one, `Named`
    /// rewrites fail honestly per statement.
    pub fn with_rewrite_resolver(mut self, natives: Arc<dyn NativeRewrite>) -> Self {
        self.natives = Some(natives);
        self
    }

    /// Register an [`EngineContextProvider`] under its
    /// [`engine_name()`](EngineContextProvider::engine_name) —
    /// satisfies `requires:` entries of that name.
    pub fn with_engine_context(mut self, provider: impl EngineContextProvider + 'static) -> Self {
        self.contexts
            .insert(provider.engine_name().to_string(), Arc::new(provider));
        self
    }

    /// Run the codemod: per target → per artifact → edits + changes.
    ///
    /// `targets` supplies the glob-resolved artifact sets (see
    /// [`TargetArtifacts`]). Deterministic: `changes` sorted by
    /// `ArtifactRef`, `edits` sorted by `(artifact, span, rule_id)`.
    ///
    /// # Errors
    ///
    /// [`CodemodError::InvalidTarget`] for a bad target index, or
    /// [`CodemodError::Engine`] when a rule's `on_engine_error: fail`
    /// fires.
    pub fn plan(
        &self,
        spec: &CodemodSpec,
        targets: &[TargetArtifacts],
    ) -> Result<CodemodPlan, CodemodError> {
        let mut plan = CodemodPlan {
            name: spec.name.clone(),
            version: spec.version,
            edits: Vec::new(),
            changes: Vec::new(),
            diagnostics: Vec::new(),
            stats: CodemodStats::default(),
        };
        let mut cache = EngineContextCache::new();

        // `scope: program` is declared but unimplemented in v1 — one
        // honest diagnostic per rule, never a silent drop.
        for rule in &spec.rules {
            if rule.scope == RuleScope::Program {
                plan.diagnostics.push(
                    CodemodDiagnostic::new(
                        DiagnosticKind::ProgramScopeUnsupported,
                        format!(
                            "rule '{}' has scope: program — cross-file rules are not implemented in v1",
                            rule.id
                        ),
                    )
                    .at_rule(rule.id.clone()),
                );
            }
        }

        for input in targets {
            if input.target >= spec.targets.len() {
                return Err(CodemodError::InvalidTarget(input.target));
            }
            let target = &spec.targets[input.target];
            self.plan_target(spec, target, input, &mut plan, &mut cache)?;
        }

        // Deterministic ordering (T-084): changes by ArtifactRef,
        // edits by (artifact, span.start, rule_id, node_kind).
        plan.changes.sort_by(|a, b| a.artifact().cmp(b.artifact()));
        plan.edits.sort_by(|a, b| {
            (
                &a.artifact,
                a.span.map(|s| s.start),
                &a.rule_id,
                &a.node_kind,
            )
                .cmp(&(
                    &b.artifact,
                    b.span.map(|s| s.start),
                    &b.rule_id,
                    &b.node_kind,
                ))
        });
        Ok(plan)
    }

    /// Plan byte-splice edits for one artifact — the lint `--fix` path
    /// (PL-004 T-091).
    ///
    /// Byte edits are *computed* desired-state, not spec rules: an
    /// upstream producer (a lint pass's `fix` payloads, formatter
    /// output, …) supplies [`ByteEdit`]s; this reads the artifact,
    /// applies them via [`apply_byte_edits`], and emits the same
    /// [`CodemodPlan`]/[`Change`] types the AST pipeline produces — so
    /// `hnl-tool-codegen`'s apply/revert lifecycle journals,
    /// drift-checks, and reverts the write with no special casing. No
    /// parse, no `unparse` mode: the splice *is* the materialization.
    ///
    /// `ByteEdit::rule` is provenance only — it lands on
    /// [`CodemodEdit::rule_id`]; `node_kind`/`rewrite` record
    /// `""`/`"splice"` since no AST node is involved. A batch that
    /// fails validation (overlap, out-of-bounds, non-char-boundary)
    /// declines the whole artifact with a [`DiagnosticKind::SpliceError`]
    /// diagnostic — never a partial splice, same policy as unparseable
    /// artifacts.
    ///
    /// The returned plan carries `name: "byte-edits"`, `version: 0` —
    /// there is no spec to take them from.
    pub fn plan_byte_edits(&self, artifact: &ArtifactRef, edits: &[ByteEdit]) -> CodemodPlan {
        let mut plan = CodemodPlan {
            name: "byte-edits".into(),
            version: 0,
            edits: Vec::new(),
            changes: Vec::new(),
            diagnostics: Vec::new(),
            stats: CodemodStats::default(),
        };
        if edits.is_empty() {
            return plan;
        }
        plan.stats.artifacts_seen += 1;
        plan.stats.edits += edits.len();

        // --- read ---------------------------------------------------
        let bytes = match self.store.read(artifact) {
            Ok(Some(b)) => b,
            Ok(None) => {
                plan.diagnostics.push(
                    CodemodDiagnostic::new(
                        DiagnosticKind::ReadError,
                        "artifact not found in store",
                    )
                    .at_artifact(artifact.clone()),
                );
                return plan;
            }
            Err(e) => {
                plan.diagnostics.push(
                    CodemodDiagnostic::new(DiagnosticKind::ReadError, e.to_string())
                        .at_artifact(artifact.clone()),
                );
                return plan;
            }
        };
        let source = match String::from_utf8(bytes.clone()) {
            Ok(s) => s,
            Err(_) => {
                plan.diagnostics.push(
                    CodemodDiagnostic::new(DiagnosticKind::NotUtf8, "artifact is not valid UTF-8")
                        .at_artifact(artifact.clone()),
                );
                return plan;
            }
        };
        let hash = ContentHash::of(&bytes);

        // --- splice --------------------------------------------------
        let text = match apply_byte_edits(&source, edits) {
            Ok(t) => t,
            Err(e) => {
                plan.diagnostics.push(
                    CodemodDiagnostic::new(DiagnosticKind::SpliceError, e.to_string())
                        .at_artifact(artifact.clone()),
                );
                return plan;
            }
        };

        // Edit records, sorted like the AST path's output.
        let mut recs: Vec<&ByteEdit> = edits.iter().collect();
        recs.sort_by_key(|e| (e.start, e.end, e.rule.as_str()));
        for e in recs {
            let span = Span {
                start: e.start,
                end: e.end,
            };
            plan.edits.push(CodemodEdit {
                artifact: artifact.clone(),
                rule_id: e.rule.clone(),
                span: Some(span),
                node_kind: String::new(),
                before: snippet(&source, span),
                after: Some(e.replacement.clone()),
                deleted: false,
                rewrite: Some("splice".into()),
            });
        }

        // --- materialize ---------------------------------------------
        if text != source {
            plan.stats.artifacts_changed += 1;
            plan.changes.push(Change::put_with_before(
                artifact.clone(),
                Some(hash),
                text.into_bytes(),
            ));
        }
        plan
    }

    /// One target: resolve language, compile rules, map artifacts.
    fn plan_target(
        &self,
        spec: &CodemodSpec,
        target: &CodemodTarget,
        input: &TargetArtifacts,
        plan: &mut CodemodPlan,
        cache: &mut EngineContextCache,
    ) -> Result<(), CodemodError> {
        let mut artifacts = input.artifacts.clone();
        artifacts.sort();
        artifacts.dedup();
        plan.stats.artifacts_seen += artifacts.len();
        if artifacts.is_empty() {
            plan.diagnostics.push(
                CodemodDiagnostic::new(
                    DiagnosticKind::NoArtifacts,
                    format!(
                        "target '{}' (glob {}) matched no artifacts",
                        target.language, target.glob
                    ),
                )
                .at_artifact(ArtifactRef::new(&target.glob)),
            );
            return Ok(());
        }

        // Language resolution (T-082): per target, via the injected
        // resolver. Failure skips the whole target honestly.
        let lang = match &self.languages {
            Some(resolver) => match resolver.resolve(&target.language) {
                Ok(l) => l,
                Err(msg) => {
                    plan.diagnostics.push(
                        CodemodDiagnostic::new(
                            DiagnosticKind::LanguageUnresolved,
                            format!("language {:?}: {msg}", target.language),
                        )
                        .at_artifact(ArtifactRef::new(&target.language)),
                    );
                    return Ok(());
                }
            },
            None => {
                plan.diagnostics.push(
                    CodemodDiagnostic::new(
                        DiagnosticKind::LanguageUnresolved,
                        "no LanguageResolver wired — cannot resolve target language",
                    )
                    .at_artifact(ArtifactRef::new(&target.language)),
                );
                return Ok(());
            }
        };

        // Compile file-scope rules once per target (defs don't depend
        // on the artifact; `requires:` gating is per artifact below).
        let (compiled, errors) = compile_rules(spec, &lang);
        for (rule_id, msg) in errors {
            plan.diagnostics
                .push(CodemodDiagnostic::new(DiagnosticKind::RuleInvalid, msg).at_rule(rule_id));
        }

        for artifact in &artifacts {
            self.plan_artifact(target, &lang, &compiled, artifact, plan, cache)?;
        }
        Ok(())
    }

    /// One artifact: read → gate → parse → rewrite → edits → change.
    fn plan_artifact(
        &self,
        target: &CodemodTarget,
        lang: &Language,
        compiled: &[CompiledRule],
        artifact: &ArtifactRef,
        plan: &mut CodemodPlan,
        cache: &mut EngineContextCache,
    ) -> Result<(), CodemodError> {
        // --- read ---------------------------------------------------
        let bytes = match self.store.read(artifact) {
            Ok(Some(b)) => b,
            Ok(None) => {
                plan.diagnostics.push(
                    CodemodDiagnostic::new(
                        DiagnosticKind::ReadError,
                        "artifact not found in store",
                    )
                    .at_artifact(artifact.clone()),
                );
                return Ok(());
            }
            Err(e) => {
                plan.diagnostics.push(
                    CodemodDiagnostic::new(DiagnosticKind::ReadError, e.to_string())
                        .at_artifact(artifact.clone()),
                );
                return Ok(());
            }
        };
        let source = match String::from_utf8(bytes.clone()) {
            Ok(s) => s,
            Err(_) => {
                plan.diagnostics.push(
                    CodemodDiagnostic::new(DiagnosticKind::NotUtf8, "artifact is not valid UTF-8")
                        .at_artifact(artifact.clone()),
                );
                return Ok(());
            }
        };
        let hash = ContentHash::of(&bytes);

        // --- requires: gating (T-083) -------------------------------
        let active = self.gate_rules(artifact, hash, &source, lang, compiled, plan, cache)?;
        if active.is_empty() {
            return Ok(());
        }
        // Rewrite defs of active rules; observe rules ride along for
        // the match walk.
        let mut defs = Vec::new();
        let mut observe: Vec<&CompiledRule> = Vec::new();
        for &i in &active {
            let cr = &compiled[i];
            if cr.rule.rewrite.is_some() {
                defs.extend(cr.defs.iter().cloned());
            } else {
                observe.push(cr);
            }
        }

        // --- split + raw-parse --------------------------------------
        let chunks = self.splitter.split(&source);
        let mut stmts: Vec<(handl_language::Ast, StatementChunk)> = Vec::new();
        let mut parse_failed = false;
        for chunk in chunks {
            match self.parse_statement(lang, &chunk) {
                Ok(Some(ast)) => stmts.push((ast, chunk)),
                Ok(None) => {} // trivia-only chunk — nothing to rewrite
                Err(msg) => {
                    plan.diagnostics.push(
                        CodemodDiagnostic::new(DiagnosticKind::ParseError, msg)
                            .at_artifact(artifact.clone()),
                    );
                    parse_failed = true;
                    break;
                }
            }
        }
        if parse_failed {
            // Codemod's on_error:skip default — an unparseable file is
            // skipped whole, never partially rewritten.
            return Ok(());
        }
        if stmts.is_empty() {
            return Ok(());
        }
        plan.stats.artifacts_parsed += 1;
        plan.stats.statements += stmts.len();

        // --- per-statement rewrite ----------------------------------
        let mut emitted: Vec<Option<String>> = Vec::with_capacity(stmts.len());
        let mut stmt_failed = false;
        for (ast, chunk) in &stmts {
            match self.rewrite_statement(
                lang,
                &source,
                artifact,
                ast,
                chunk,
                &defs,
                &observe,
                compiled,
                &target.unparse,
                plan,
            ) {
                Ok(outcome) => {
                    plan.stats.edits += outcome.edits.len();
                    plan.edits.extend(outcome.edits);
                    plan.stats.rewrites += outcome.rewrites;
                    emitted.push(outcome.text);
                }
                Err(()) => {
                    stmt_failed = true;
                    // Fall back to the verbatim slice — partial
                    // application with an honest diagnostic beats a
                    // dropped statement.
                    emitted.push(Some(source[chunk.offset..chunk.end()].to_string()));
                }
            }
        }
        let _ = stmt_failed;

        // --- materialize --------------------------------------------
        if target.unparse == UnparseMode::None {
            return Ok(()); // plan-only: edits recorded, no Change
        }
        let Some(text) = assemble(&source, &stmts, &emitted) else {
            return Ok(());
        };
        if text != source {
            plan.stats.artifacts_changed += 1;
            plan.changes.push(Change::put_with_before(
                artifact.clone(),
                Some(hash),
                text.into_bytes(),
            ));
        }
        Ok(())
    }

    /// `requires:` gate: returns the indices of compiled rules allowed
    /// on this artifact. Engine failures honor `on_engine_error`.
    #[allow(clippy::too_many_arguments)]
    fn gate_rules(
        &self,
        artifact: &ArtifactRef,
        hash: ContentHash,
        source: &str,
        lang: &Language,
        compiled: &[CompiledRule],
        plan: &mut CodemodPlan,
        cache: &mut EngineContextCache,
    ) -> Result<Vec<usize>, CodemodError> {
        let mut active = Vec::new();
        'rules: for (i, cr) in compiled.iter().enumerate() {
            for engine in &cr.rule.requires {
                let before = cache.len();
                match context_for(&self.contexts, cache, artifact, hash, source, lang, engine) {
                    Ok(_) => {
                        plan.stats.engine_context_calls += cache.len() - before;
                    }
                    Err(msg) => {
                        plan.stats.engine_context_calls += cache.len() - before;
                        match cr.rule.on_engine_error {
                            EngineErrorPolicy::Fail => {
                                return Err(CodemodError::Engine {
                                    artifact: artifact.clone(),
                                    rule_id: cr.rule.id.clone(),
                                    engine: engine.clone(),
                                    message: msg,
                                });
                            }
                            _ => {
                                plan.stats.rules_skipped_engine += 1;
                                plan.diagnostics.push(
                                    CodemodDiagnostic::new(
                                        DiagnosticKind::EngineUnavailable,
                                        format!("requires '{engine}': {msg}"),
                                    )
                                    .at_artifact(artifact.clone())
                                    .at_rule(cr.rule.id.clone())
                                    .at_engine(engine.clone()),
                                );
                                continue 'rules;
                            }
                        }
                    }
                }
            }
            active.push(i);
        }
        Ok(active)
    }

    /// Lex + Pratt-parse one statement chunk; spans re-offset into the
    /// file source (the `ext-transform` recipe — raw parse, no
    /// desugar).
    fn parse_statement(
        &self,
        lang: &Language,
        chunk: &StatementChunk,
    ) -> Result<Option<handl_language::Ast>, String> {
        let tokens =
            hnl_tool_lex_pattern::lex(lang, &chunk.text).map_err(|e| format!("lex: {e}"))?;
        if tokens.as_slice().is_empty() {
            return Ok(None);
        }
        let req = hnl_tool_parse::ParserRequest::new(tokens);
        let mut ast = hnl_tool_parse_pratt::PrattEngine::new()
            .parse(lang, &req)
            .map_err(|e| format!("parse: {e}"))?;
        if ast.is_empty() {
            return Ok(None);
        }
        // Re-offset spans into the file so PreserveEmitter splices from
        // the whole-source `src`.
        for i in 0..ast.len() {
            let r = AstRef::new(i);
            let node = ast
                .get_mut(r)
                .ok_or_else(|| format!("ast index {i} out of bounds"))?;
            if let Some(span) = node.span.as_mut() {
                span.start += chunk.offset;
                span.end += chunk.offset;
            }
        }
        Ok(Some(ast))
    }

    /// Rewrite one statement arena: run the pass, compact tombstones,
    /// collect edits (rewrite trace + observe matches), and emit per
    /// the target's `unparse` mode.
    ///
    /// Returns `Err(())` on pass/emit failure — the caller substitutes
    /// the verbatim source slice (partial application, diagnosed).
    #[allow(clippy::too_many_arguments)]
    fn rewrite_statement(
        &self,
        lang: &Language,
        src: &str,
        artifact: &ArtifactRef,
        ast: &Ast,
        chunk: &StatementChunk,
        defs: &[handl_language::TransformDef],
        observe: &[&CompiledRule],
        compiled: &[CompiledRule],
        unparse: &UnparseMode,
        plan: &mut CodemodPlan,
    ) -> Result<StmtOutcome, ()> {
        let root = AstRef::new(ast.len() - 1);
        let reachable = reachable_from(ast, root);

        // --- rewrite pass -------------------------------------------
        let mut out_ast = ast.clone();
        let mut out_root = root;
        let mut trace: Vec<TraceEntry> = Vec::new();
        if !defs.is_empty() {
            let rules = CodemodRuleSource::new(defs.to_vec());
            let engine = RewriteEngine::with_rule_source(Box::new(rules))
                .with_native_registry(codemod_natives(self.natives.clone()));
            match engine.run_pass(lang, ast, root, CODEMOD_PASS) {
                Ok(out) => {
                    trace = out
                        .trace
                        .rewrites
                        .iter()
                        .map(|r| TraceEntry {
                            from: r.from,
                            to: r.to,
                            to_kind: out
                                .ast
                                .get(r.to)
                                .map(|n| n.kind.clone())
                                .unwrap_or_default(),
                            rule_id: r.rule_id.clone(),
                        })
                        .collect();
                    out_ast = out.ast;
                    out_root = out.root;
                }
                Err(e) => {
                    plan.diagnostics.push(
                        CodemodDiagnostic::new(DiagnosticKind::RewriteError, e.to_string())
                            .at_artifact(artifact.clone()),
                    );
                    return Err(());
                }
            }
        }

        // --- tombstone compaction ------------------------------------
        let compacted = match strip_tombstones(&out_ast, out_root) {
            Ok(c) => c,
            Err(e) => {
                plan.diagnostics.push(
                    CodemodDiagnostic::new(DiagnosticKind::RewriteError, e.to_string())
                        .at_artifact(artifact.clone()),
                );
                return Err(());
            }
        };

        // --- edits ----------------------------------------------------
        let rule_desc: HashMap<&str, &Option<String>> = compiled
            .iter()
            .map(|c| (c.rule.id.as_str(), &c.rewrite_desc))
            .collect();
        let mut edits = self.trace_edits(
            lang, src, artifact, ast, &trace, &compacted, &reachable, &rule_desc,
        );
        edits.extend(self.observe_edits(lang, src, artifact, ast, observe));

        // --- emit -----------------------------------------------------
        let text = self.emit_statement(
            lang, src, artifact, chunk, &compacted, &trace, unparse, plan,
        )?;

        Ok(StmtOutcome {
            text,
            edits,
            rewrites: trace.len(),
        })
    }

    /// Build [`CodemodEdit`]s from the rewrite trace (input-arena
    /// `from` refs, output-arena `to` refs).
    #[allow(clippy::too_many_arguments)]
    fn trace_edits(
        &self,
        lang: &Language,
        src: &str,
        artifact: &ArtifactRef,
        input: &Ast,
        trace: &[TraceEntry],
        compacted: &crate::compact::Compacted,
        reachable: &std::collections::BTreeSet<usize>,
        rule_desc: &HashMap<&str, &Option<String>>,
    ) -> Vec<CodemodEdit> {
        let fmt = FormatEngine::new();
        let mut edits = Vec::new();
        for entry in trace {
            if !reachable.contains(&entry.from.index()) {
                continue;
            }
            let node = input.get(entry.from);
            let span = node.and_then(|n| n.span).map(Span::from);
            let before = span.and_then(|s| snippet(src, s));
            let deleted = entry.to_kind == TOMBSTONE_KIND;
            let after = if deleted || compacted.root.is_none() {
                None
            } else {
                compacted
                    .old_to_new
                    .get(entry.to.index())
                    .copied()
                    .flatten()
                    .and_then(|new_idx| {
                        fmt.format(lang, &compacted.ast, AstRef::new(new_idx))
                            .ok()
                            .map(|s| s.trim_end().to_string())
                    })
            };
            edits.push(CodemodEdit {
                artifact: artifact.clone(),
                rule_id: entry.rule_id.clone(),
                span,
                node_kind: node.map(|n| n.kind.clone()).unwrap_or_default(),
                before,
                after,
                deleted,
                rewrite: rule_desc
                    .get(entry.rule_id.as_str())
                    .and_then(|d| (*d).clone()),
            });
        }
        edits
    }

    /// Observe-rule (no `rewrite:`) match edits.
    fn observe_edits(
        &self,
        lang: &Language,
        src: &str,
        artifact: &ArtifactRef,
        ast: &Ast,
        observe: &[&CompiledRule],
    ) -> Vec<CodemodEdit> {
        let root = AstRef::new(ast.len() - 1);
        let mut edits = Vec::new();
        for m in observe_matches(lang, ast, root, observe) {
            let node = ast.get(m.at);
            let span = node.and_then(|n| n.span).map(Span::from);
            let rule = observe[m.rule_idx];
            edits.push(CodemodEdit {
                artifact: artifact.clone(),
                rule_id: rule.rule.id.clone(),
                span,
                node_kind: node.map(|n| n.kind.clone()).unwrap_or_default(),
                before: span.and_then(|s| snippet(src, s)),
                after: None,
                deleted: false,
                rewrite: None,
            });
        }
        edits
    }

    /// Emit one statement's text per the target `unparse` mode —
    /// `None` for [`UnparseMode::None`].
    #[allow(clippy::too_many_arguments)]
    fn emit_statement(
        &self,
        lang: &Language,
        src: &str,
        artifact: &ArtifactRef,
        chunk: &StatementChunk,
        compacted: &crate::compact::Compacted,
        trace: &[TraceEntry],
        unparse: &UnparseMode,
        plan: &mut CodemodPlan,
    ) -> Result<Option<String>, ()> {
        match unparse {
            UnparseMode::None => Ok(None),
            UnparseMode::Preserve => {
                if trace.is_empty() {
                    // No rewrites — the statement's source slice is
                    // already canonical.
                    return Ok(Some(src[chunk.offset..chunk.end()].to_string()));
                }
                match compacted.root {
                    None => Ok(Some(String::new())), // whole stmt deleted
                    Some(root) => {
                        // `changed` seeds: every rewritten output node
                        // (post-compaction index) plus parents that
                        // shrank when tombstones were stripped — both
                        // must re-emit rather than splice stale source.
                        let mut changed: Vec<AstRef> = Vec::new();
                        for e in trace {
                            if let Some(Some(ni)) = compacted.old_to_new.get(e.to.index()) {
                                changed.push(AstRef::new(*ni));
                            }
                        }
                        for &p in &compacted.shrank_parents {
                            if let Some(Some(ni)) = compacted.old_to_new.get(p) {
                                changed.push(AstRef::new(*ni));
                            }
                        }
                        let emitter = PreserveEmitter::new(src, &compacted.ast, root, &changed)
                            .map_err(|e| self.emit_diag(plan, artifact, e.to_string()))?;
                        emitter
                            .format(lang, &compacted.ast, root)
                            .map(Some)
                            .map_err(|e| self.emit_diag(plan, artifact, e.to_string()))
                    }
                }
            }
            UnparseMode::Full => match compacted.root {
                None => Ok(Some(String::new())),
                Some(root) => FormatEngine::new()
                    .format(lang, &compacted.ast, root)
                    .map(Some)
                    .map_err(|e| self.emit_diag(plan, artifact, e.to_string())),
            },
            // `validate` rejects Unknown spellings; an unvalidated spec
            // reaching the engine declines honestly per statement.
            UnparseMode::Unknown(v) => {
                self.emit_diag(
                    plan,
                    artifact,
                    format!("unknown unparse mode {v:?} (spec skipped validation)"),
                );
                Err(())
            }
        }
    }

    fn emit_diag(&self, plan: &mut CodemodPlan, artifact: &ArtifactRef, msg: String) {
        plan.diagnostics.push(
            CodemodDiagnostic::new(DiagnosticKind::EmitError, msg).at_artifact(artifact.clone()),
        );
    }
}

// -- internals --------------------------------------------------------

/// Per-statement outcome of [`CodemodEngine::rewrite_statement`].
struct StmtOutcome {
    /// Emitted statement text (`None` for `unparse: none` targets or
    /// deleted statements emit `""`).
    text: Option<String>,
    /// Edits collected for this statement.
    edits: Vec<CodemodEdit>,
    /// Rewrite count (stats).
    rewrites: usize,
}

/// A trace entry with both arena refs and the output node kind —
/// carried so `deleted` detection doesn't need the pre-compaction
/// arena around.
struct TraceEntry {
    from: AstRef,
    to: AstRef,
    to_kind: String,
    rule_id: String,
}

fn snippet(src: &str, span: Span) -> Option<String> {
    (span.start <= span.end && span.end <= src.len()).then(|| src[span.start..span.end].to_string())
}

/// Assemble the artifact text from per-statement emissions, splicing
/// the inter-statement gaps verbatim (comments, blank lines). `None`
/// when any statement has no emission (only possible for
/// [`UnparseMode::None`], which returns before this is called).
fn assemble(
    src: &str,
    stmts: &[(Ast, StatementChunk)],
    emitted: &[Option<String>],
) -> Option<String> {
    if stmts.is_empty() {
        return None;
    }
    let mut out = String::new();
    out.push_str(&src[..stmts[0].1.offset]);
    for (i, ((_, chunk), text)) in stmts.iter().zip(emitted.iter()).enumerate() {
        out.push_str(text.as_deref()?);
        if i + 1 < stmts.len() {
            out.push_str(&src[chunk.end()..stmts[i + 1].1.offset]);
        } else {
            out.push_str(&src[chunk.end()..]);
        }
    }
    Some(out)
}
